//! Upstream-подключения: запуск, health-check, автоматический failover.
//!
//! Поддерживаемые типы:
//! - [`Wg`](UpstreamKind::Wg) — WireGuard (`wg-quick`) и AmneziaWG (`awg-quick`)
//! - [`Xray`](UpstreamKind::Xray) — через `xray run` (TUN-режим)
//!
//! ## Добавление нового протокола
//! 1. Создать структуру в `upstream/<type>.rs`
//! 2. Добавить вариант в [`UpstreamKind`]
//! 3. Добавить match-ветки в методы `UpstreamKind`
//! 4. Добавить тип в [`UpstreamType`] и в [`create_upstream`]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::config::{Config, SpeedTestConfig, UpstreamConfig, UpstreamType};
use crate::monitor::{self, Monitor, SharedMetrics, SwitchDecision, SwitchReason};
use crate::nat::NatSetup;
use crate::routing::{ExitRoute, RoutingSetup};

mod wireguard;
mod xray;

use wireguard::WgUpstream;
use xray::XrayUpstream;

pub enum UpstreamKind {
    Wg(WgUpstream),
    Xray(XrayUpstream),
}

impl UpstreamKind {
    pub fn name(&self) -> &str {
        match self {
            Self::Wg(w) => w.name(),
            Self::Xray(x) => x.name(),
        }
    }
    pub fn interface(&self) -> &str {
        match self {
            Self::Wg(w) => w.interface(),
            Self::Xray(x) => x.interface(),
        }
    }
    pub fn health_check_interval(&self) -> Duration {
        match self {
            Self::Wg(w) => w.health_check_interval(),
            Self::Xray(x) => x.health_check_interval(),
        }
    }
    pub fn failover_threshold(&self) -> u32 {
        match self {
            Self::Wg(w) => w.failover_threshold(),
            Self::Xray(x) => x.failover_threshold(),
        }
    }
    pub async fn start(&self) -> Result<()> {
        match self {
            Self::Wg(w) => w.start().await,
            Self::Xray(x) => x.start().await,
        }
    }
    pub async fn stop(&self) -> Result<()> {
        match self {
            Self::Wg(w) => w.stop().await,
            Self::Xray(x) => x.stop().await,
        }
    }
    pub async fn health_check(&self) -> Result<()> {
        match self {
            Self::Wg(w) => w.health_check().await,
            Self::Xray(x) => x.health_check().await,
        }
    }
}

fn create_upstream(config: &UpstreamConfig) -> Result<UpstreamKind> {
    match config.upstream_type {
        UpstreamType::Wireguard => Ok(UpstreamKind::Wg(WgUpstream::new(config, "wg-quick")?)),
        UpstreamType::Amneziawg => Ok(UpstreamKind::Wg(WgUpstream::new(config, "awg-quick")?)),
        UpstreamType::Xray => Ok(UpstreamKind::Xray(XrayUpstream::new(config)?)),
    }
}

pub struct UpstreamManager {
    upstreams: Vec<Arc<UpstreamKind>>,
    recovery_tasks: Vec<Option<JoinHandle<Result<()>>>>,
    ready: Vec<bool>,
    active_index: Option<usize>,
    failure_count: u32,
    routing: Option<Arc<RoutingSetup>>,
    nat: Option<NatSetup>,
    server_interface: String,
    server_subnet: String,
    use_nftables: bool,
    masquerade: bool,
    exit_route: Option<ExitRoute>,
    direct_fallback: bool,
    outage_since: Option<std::time::Instant>,
    last_outage_log: Option<std::time::Instant>,
    monitor: Option<Monitor>,
    shared_metrics: SharedMetrics,
    speed_test: SpeedTestConfig,
    speed_selector: monitor::Selector,
    health_task: Option<JoinHandle<Vec<monitor::HealthProbeOutcome>>>,
    speed_task: Option<JoinHandle<Vec<SpeedMeasurement>>>,
    https_task: Option<JoinHandle<Vec<monitor::HttpsProbeOutcome>>>,
}

struct SpeedMeasurement {
    index: usize,
    result: Result<monitor::QualityResult>,
}

impl UpstreamManager {
    pub async fn start(config: &Config) -> Result<Self> {
        let upstreams: Vec<Arc<UpstreamKind>> = config
            .upstream
            .iter()
            .filter(|u| u.enabled)
            .map(|c| create_upstream(c).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;

        let routing = RoutingSetup::apply(config).await?;
        let shared_metrics: SharedMetrics =
            std::sync::Arc::new(std::sync::Mutex::new(monitor::MetricsSnapshot::default()));
        let monitor = if config.monitoring.enabled {
            Some(Monitor::new(config))
        } else {
            None
        };

        let mut manager = Self {
            ready: vec![false; upstreams.len()],
            recovery_tasks: (0..upstreams.len()).map(|_| None).collect(),
            upstreams,
            active_index: None,
            failure_count: 0,
            routing: Some(Arc::new(routing)),
            nat: None,
            server_interface: config.server.interface.clone(),
            server_subnet: config.server.subnet.to_string(),
            use_nftables: config.nat.use_nftables,
            masquerade: config.nat.masquerade,
            exit_route: None,
            direct_fallback: false,
            outage_since: None,
            last_outage_log: None,
            monitor,
            shared_metrics: shared_metrics.clone(),
            speed_test: if !config.speed_test.enabled
                && config.monitoring.enabled
                && config.monitoring.quality_enabled
            {
                SpeedTestConfig {
                    enabled: true,
                    url: config.monitoring.quality_url.clone(),
                    interval: config.monitoring.quality_interval,
                    timeout: config.monitoring.quality_timeout,
                    min_mbps: 0.0,
                }
            } else {
                config.speed_test.clone()
            },
            speed_selector: monitor::Selector::new(monitor::SelectorConfig {
                enabled: true,
                ..config.monitoring.selector.clone()
            }),
            health_task: None,
            speed_task: None,
            https_task: None,
        };

        info!(event = "selection_config", monitoring_enabled = config.monitoring.enabled,
              selector = ?config.monitoring.selector,
              speed_test_enabled = manager.speed_test.enabled,
              speed_interval_secs = manager.speed_test.interval,
              speed_timeout_secs = manager.speed_test.timeout,
              min_mbps = manager.speed_test.min_mbps, download_used_in_score = false,
              "настройки выбора upstream");
        if config.monitoring.enabled {
            info!(event = "monitoring_config", health_targets = ?config.monitoring.health_targets,
                  health_interval_secs = config.monitoring.health_interval,
                  health_timeout_secs = config.monitoring.health_timeout,
                  health_probe_count = config.monitoring.health_probe_count,
                  https_enabled = config.monitoring.https_enabled,
                  https_target_count = config.monitoring.https_targets.len(),
                  https_interval_secs = config.monitoring.https_interval,
                  https_timeout_secs = config.monitoring.https_timeout,
                  ewma_alpha = config.monitoring.ewma_alpha, scoring = ?config.monitoring.scoring,
                  "настройки проб и scoring");
        }
        for upstream in config.upstream.iter().filter(|u| u.enabled) {
            info!(event = "upstream_health_config", upstream = %upstream.name,
                  interface = %upstream.interface, target = %upstream.health_check_target,
                  interval_secs = upstream.health_check_interval,
                  timeout_secs = upstream.health_check_timeout, threshold = upstream.failover_threshold,
                  "настройки обычного health-check");
        }

        for i in 0..manager.upstreams.len() {
            manager.request_recovery(i);
        }
        for i in 0..manager.upstreams.len() {
            manager.finish_recovery(i).await;
        }
        if let Err(error) = manager.select_initial().await {
            if let Err(cleanup) = manager.shutdown().await {
                return Err(error.context(format!("ошибка очистки: {cleanup:#}")));
            }
            return Err(error);
        }

        if let Some(ref mut m) = manager.monitor {
            if let Some(idx) = manager.active_index {
                m.set_selector_active(idx);
            }
        }

        manager.publish_metrics();
        Ok(manager)
    }

    pub fn shared_metrics(&self) -> SharedMetrics {
        self.shared_metrics.clone()
    }
    pub fn prometheus_enabled(&self) -> bool {
        self.monitor
            .as_ref()
            .map(|m| m.prometheus_enabled())
            .unwrap_or(false)
    }
    pub fn prometheus_port(&self) -> u16 {
        self.monitor
            .as_ref()
            .map(|m| m.prometheus_port())
            .unwrap_or(9099)
    }

    fn publish_metrics(&self) {
        *self.shared_metrics.lock().unwrap() = monitor::MetricsSnapshot {
            vpns: self
                .monitor
                .as_ref()
                .map(|m| m.snapshot(self.active_index))
                .unwrap_or_default(),
            direct_fallback: self.direct_fallback,
        };
    }

    async fn select_initial(&mut self) -> Result<()> {
        for i in 0..self.upstreams.len() {
            if !self.ready[i] {
                continue;
            }
            let upstream = &self.upstreams[i];
            match upstream.health_check().await {
                Ok(()) => match self.activate(i).await {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        warn!(error = %error, "начальная активация не удалась; пробуем следующий upstream")
                    }
                },
                Err(e) => {
                    warn!(upstream = upstream.name(), error = %e, "health check не прошёл");
                }
            }
        }
        self.enter_outage().await;
        Ok(())
    }

    async fn configure_exit(&mut self, route: ExitRoute) -> Result<()> {
        let result: Result<()> = async {
            if let Some(routing) = &self.routing {
                routing.set_exit_route(&route).await?;
            }
            if let Some(nat) = self.nat.as_mut() {
                nat.switch_interface(&route.interface).await?;
            } else {
                self.nat = Some(
                    NatSetup::apply(
                        &self.server_interface,
                        &route.interface,
                        &self.server_subnet,
                        self.use_nftables,
                        self.masquerade,
                    )
                    .await?,
                );
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let rollback: Result<()> = async {
                if let (Some(previous), Some(routing)) = (&self.exit_route, &self.routing) {
                    if let Some(nat) = &mut self.nat {
                        nat.switch_interface(&previous.interface).await?;
                    }
                    routing.set_exit_route(previous).await?;
                } else if let Some(routing) = &self.routing {
                    routing.suspend().await?;
                }
                Ok(())
            }
            .await;
            if let Err(rollback) = rollback {
                self.active_index = None;
                self.direct_fallback = false;
                self.exit_route = None;
                if let Some(routing) = &self.routing {
                    let _ = routing.suspend().await;
                }
                return Err(error.context(format!(
                    "восстановление NAT/маршрута также не удалось: {rollback}"
                )));
            }
            return Err(error);
        }
        self.exit_route = Some(route);
        Ok(())
    }

    async fn activate(&mut self, index: usize) -> Result<()> {
        let interface = self.upstreams[index].interface().to_string();
        let name = self.upstreams[index].name().to_string();
        self.configure_exit(ExitRoute {
            interface: interface.clone(),
            gateway: None,
        })
        .await?;
        self.active_index = Some(index);
        self.speed_selector.set_active(index);
        self.direct_fallback = false;
        self.failure_count = 0;
        if let Some(m) = &mut self.monitor {
            m.set_selector_active(index);
        }
        if let Some(since) = self.outage_since.take() {
            info!(event = "vpn_restored", upstream = %name, outage_secs = since.elapsed().as_secs(), "VPN восстановлен; прямой выход отключён");
        }
        self.last_outage_log = None;
        info!(event = "vpn_activated", upstream = %name, interface = %interface, "активный upstream");
        self.publish_metrics();
        Ok(())
    }

    async fn enter_outage(&mut self) {
        self.active_index = None;
        self.speed_selector.clear_active();
        if let Some(m) = &mut self.monitor {
            m.clear_selector_active();
        }
        if self.outage_since.is_none() {
            self.outage_since = Some(std::time::Instant::now());
            self.last_outage_log = self.outage_since;
            error!(event = "vpn_unavailable", "Нет доступного VPN: все включённые upstream недоступны или не удалось настроить их маршруты");
        }
        self.ensure_direct_fallback().await;
    }

    async fn ensure_direct_fallback(&mut self) {
        let result: Result<()> = async {
            let route = self.routing.as_ref().unwrap().direct_route(&self.server_interface).await?;
            if !self.direct_fallback || self.exit_route.as_ref() != Some(&route) {
                self.configure_exit(route.clone()).await?;
                self.direct_fallback = true;
                warn!(event = "direct_fallback_enabled", interface = %route.interface, gateway = ?route.gateway,
                      "VPN недоступен; включён прямой выход через основной интерфейс без VPN");
            } else {
                self.routing.as_ref().unwrap().set_exit_route(&route).await?;
            }
            Ok(())
        }.await;
        if let Err(e) = result {
            self.direct_fallback = false;
            self.exit_route = None;
            if let Some(routing) = &self.routing {
                let _ = routing.suspend().await;
            }
            error!(event = "direct_fallback_failed", error = %format!("{e:#}"), "VPN недоступен; прямой выход также не настроен, клиентам интернет недоступен");
        }
        self.publish_metrics();
        if self
            .last_outage_log
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
        {
            warn!(
                event = "vpn_still_unavailable",
                direct_fallback = self.direct_fallback,
                retry_secs = 10,
                "VPN всё ещё недоступен; продолжаем попытки восстановления"
            );
            self.last_outage_log = Some(std::time::Instant::now());
        }
    }

    fn request_recovery(&mut self, index: usize) {
        if self.recovery_tasks[index].is_none() {
            let upstream = self.upstreams[index].clone();
            let routing = self.routing.clone();
            self.recovery_tasks[index] = Some(tokio::spawn(async move {
                upstream.start().await?;
                if let Some(routing) = routing {
                    routing.prepare_probe_route(upstream.interface()).await?;
                }
                Ok(())
            }));
        }
    }

    async fn finish_recovery(&mut self, index: usize) {
        let Some(task) = self.recovery_tasks[index].take() else {
            return;
        };
        let result = task
            .await
            .context("recovery task")
            .and_then(|result| result);
        self.ready[index] = result.is_ok();
        if let Err(error) = result {
            warn!(upstream = self.upstreams[index].name(), error = %error, "восстановление upstream не удалось");
        }
    }

    fn ensure_upstream_ready(&mut self, index: usize) -> Result<()> {
        if !self.ready[index] {
            self.request_recovery(index);
            anyhow::bail!("восстановление upstream выполняется в фоне");
        }
        Ok(())
    }

    pub async fn monitor_loop(&mut self) {
        let mut last_health: Option<std::time::Instant> = None;
        let mut last_monitor: Option<std::time::Instant> = None;
        let mut last_https: Option<std::time::Instant> = None;
        let mut last_speed: Option<std::time::Instant> = None;
        let mut last_recovery: Option<std::time::Instant> = None;
        loop {
            for i in 0..self.upstreams.len() {
                if self.recovery_tasks[i]
                    .as_ref()
                    .is_some_and(JoinHandle::is_finished)
                {
                    self.finish_recovery(i).await;
                }
            }
            if last_recovery.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
                for i in 0..self.upstreams.len() {
                    self.request_recovery(i);
                }
                last_recovery = Some(std::time::Instant::now());
            }
            if self
                .speed_task
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
            {
                match self.speed_task.take().unwrap().await {
                    Ok(measurements) => self.apply_speed_tests(measurements).await,
                    Err(error) => error!(error = %error, "speed test task завершилась"),
                }
            }
            if self
                .https_task
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
            {
                match self.https_task.take().unwrap().await {
                    Ok(outcomes) => self
                        .monitor
                        .as_mut()
                        .expect("HTTPS task requires monitor")
                        .apply_https_probes(outcomes),
                    Err(error) => error!(error = %error, "HTTPS-пробы task завершилась"),
                }
            }
            let health_interval = self
                .active_index
                .map(|i| self.upstreams[i].health_check_interval())
                .unwrap_or(Duration::from_secs(10));
            if last_health.is_none_or(|t| t.elapsed() >= health_interval) {
                self.check_active().await;
                last_health = Some(std::time::Instant::now());
            }
            let monitor_due = self.monitor.as_ref().is_some_and(|m| {
                last_monitor
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(m.health_interval_secs()))
            });
            if self
                .health_task
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
            {
                match self.health_task.take().unwrap().await {
                    Ok(outcomes) => {
                        if let Some(decision) = self
                            .monitor
                            .as_mut()
                            .unwrap()
                            .apply_health_probes(outcomes, self.active_index)
                        {
                            self.handle_switch_decision(decision, "health_score").await;
                        }
                    }
                    Err(error) => error!(error = %error, "ICMP task завершилась"),
                }
            }
            if monitor_due && self.health_task.is_none() {
                for i in 0..self.upstreams.len() {
                    self.request_recovery(i);
                }
                self.health_task = Some(self.monitor.as_ref().unwrap().start_health_probes());
                last_monitor = Some(std::time::Instant::now());
            }
            let https_due = self.monitor.as_ref().is_some_and(|m| {
                m.https_enabled()
                    && last_https
                        .is_none_or(|t| t.elapsed() >= Duration::from_secs(m.https_interval_secs()))
            });
            if https_due && self.https_task.is_none() {
                self.https_task = Some(
                    self.monitor
                        .as_ref()
                        .expect("HTTPS task requires monitor")
                        .start_https_probes(),
                );
                last_https = Some(std::time::Instant::now());
            }
            if self.speed_test.enabled
                && self.monitor.as_ref().is_none_or(|m| m.has_health_results())
                && self.speed_task.is_none()
                && last_speed
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(self.speed_test.interval))
            {
                self.start_speed_tests();
                last_speed = Some(std::time::Instant::now());
            }
            self.publish_metrics();
            time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn start_speed_tests(&mut self) {
        let mut interfaces = Vec::new();
        for i in 0..self.upstreams.len() {
            if self
                .monitor
                .as_ref()
                .is_some_and(|monitor| monitor.is_unhealthy(i))
            {
                debug!(
                    upstream = self.upstreams[i].name(),
                    "speed test пропущен: upstream UNHEALTHY"
                );
                continue;
            }
            if let Err(e) = self.ensure_upstream_ready(i) {
                warn!(upstream = self.upstreams[i].name(), error = %e, "speed test: upstream недоступен");
                continue;
            }
            interfaces.push((i, self.upstreams[i].interface().to_string()));
        }
        let url = self.speed_test.url.clone();
        let timeout = self.speed_test.timeout;
        self.speed_task = Some(tokio::spawn(async move {
            let mut measurements = Vec::with_capacity(interfaces.len());
            for (index, interface) in interfaces {
                measurements.push(SpeedMeasurement {
                    index,
                    result: monitor::quality_probe(&interface, &url, timeout).await,
                });
            }
            measurements
        }));
    }

    async fn apply_speed_tests(&mut self, measurements: Vec<SpeedMeasurement>) {
        let mut speeds = vec![None; self.upstreams.len()];
        for measurement in measurements {
            let upstream = &self.upstreams[measurement.index];
            match measurement.result {
                Ok(result) if result.success => {
                    speeds[measurement.index] = Some(result.mbps);
                    info!(upstream = upstream.name(), mbps = result.mbps, "speed test");
                }
                Ok(_) => {
                    warn!(upstream = upstream.name(), "speed test не вернул скорость");
                }
                Err(error) => {
                    warn!(upstream = upstream.name(), error = %error, "speed test failed");
                }
            }
        }
        if let Some(m) = &mut self.monitor {
            for (i, speed) in speeds.iter().enumerate() {
                m.record_speed(i, *speed);
            }
        }
        if let Some(active) = self.active_index {
            let eligible = speed_candidate(&speeds, active, self.speed_test.min_mbps);
            let scores: Vec<_> = speeds
                .iter()
                .enumerate()
                .filter_map(|(i, speed)| {
                    speed.map(|speed| {
                        (
                            i,
                            self.upstreams[i].name(),
                            speed,
                            monitor::VpnState::Healthy,
                        )
                    })
                })
                .collect();
            if eligible.is_none() {
                let current = speeds[active];
                let reason = if self.speed_test.min_mbps <= 0.0 {
                    "diagnostic_only"
                } else if current.is_none() {
                    "active_measurement_missing"
                } else if current.is_some_and(|speed| speed >= self.speed_test.min_mbps) {
                    "active_speed_sufficient"
                } else {
                    "no_faster_candidate"
                };
                debug!(event = "speed_switch_skipped", reason, active = self.upstreams[active].name(),
                       active_mbps = ?current, min_mbps = self.speed_test.min_mbps,
                       "speed test: переключение пропущено");
                self.speed_selector.set_active(active); // reset consecutive speed wins
            } else if let Some(decision) =
                tracing::info_span!("selector", source = "download_speed")
                    .in_scope(|| self.speed_selector.evaluate(&scores))
            {
                info!(from = %decision.from_name, to = %decision.to_name, "speed test: подтверждён кандидат на переключение");
                self.handle_switch_decision(decision, "download_speed")
                    .await;
            }
        }
    }

    async fn handle_switch_decision(&mut self, d: SwitchDecision, source: &str) {
        let to_idx = d.to;
        let from_idx = d.from;

        match &d.reason {
            SwitchReason::QualityAdvantage {
                consecutive_wins,
                advantage_pct,
            } => {
                info!(source, from = %d.from_name, to = %d.to_name, from_score = format!("{:.1}", d.from_score),
                      to_score = format!("{:.1}", d.to_score), advantage_pct = format!("{:.1}%", advantage_pct),
                      consecutive_wins, reason = "quality_advantage", "vpn_switch_requested");
            }
            SwitchReason::EmergencyFailover => {
                warn!(source, from = %d.from_name, to = %d.to_name, from_score = format!("{:.1}", d.from_score),
                      to_score = format!("{:.1}", d.to_score), reason = "emergency_failover", "vpn_switch_requested");
            }
        }

        if let Err(e) = self.ensure_upstream_ready(to_idx) {
            warn!(candidate = %d.to_name, error = %e, "preflight start failed");
            if let Some(ref mut m) = self.monitor {
                m.set_selector_active(from_idx);
            }
            return;
        }
        if let Err(e) = self.upstreams[to_idx].health_check().await {
            warn!(candidate = %d.to_name, error = %e, "preflight failed — переключение отменено");
            if let Some(ref mut m) = self.monitor {
                m.set_selector_active(from_idx);
            }
            return;
        }

        let old_index = self.active_index;
        if let Err(e) = self.activate(to_idx).await {
            error!(upstream = %d.to_name, error = %e, "ошибка активации при switch");
            if let Some(ref mut m) = self.monitor {
                m.set_selector_active(from_idx);
            }
            return;
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
        let active_interface = self.upstreams[to_idx].interface().to_string();
        match self.upstreams[to_idx].health_check().await {
            Ok(_) => {
                info!(event = "vpn_switch_verified", source, to = %d.to_name,
                      interface = %active_interface, verification = "post_activation_ping",
                      "verify passed — переключение подтверждено");
            }
            Err(e) => {
                error!(to = %d.to_name, interface = %active_interface, error = %e, "verify failed — rollback");
                if let Some(prev) = old_index {
                    if self.upstreams[prev].health_check().await.is_ok()
                        && self.activate(prev).await.is_ok()
                    {
                        return;
                    }
                }
                self.failover().await;
            }
        }
    }

    async fn check_active(&mut self) {
        let Some(active_index) = self.active_index else {
            if let Err(e) = self.try_recover().await {
                debug!(error = %e, "ожидаем восстановления upstream");
                self.enter_outage().await;
            }
            return;
        };
        match self.upstreams[active_index].health_check().await {
            Ok(()) => {
                if self.failure_count > 0 {
                    info!(
                        event = "upstream_recovered",
                        upstream = self.upstreams[active_index].name(),
                        "upstream снова отвечает"
                    );
                }
                self.failure_count = 0;
            }
            Err(e) => {
                self.failure_count = self.failure_count.saturating_add(1);
                let threshold = self.upstreams[active_index].failover_threshold();
                warn!(upstream = self.upstreams[active_index].name(), error = %e, failures = self.failure_count, threshold, "health check failed");
                if self.failure_count >= threshold {
                    self.failover().await;
                }
            }
        }
    }

    async fn failover(&mut self) {
        let previous = self
            .active_index
            .map(|i| self.upstreams[i].name().to_string());
        warn!(event = "vpn_failover_started", from = ?previous, failures = self.failure_count,
              "аварийный поиск доступного upstream");
        if let Err(e) = self.try_recover().await {
            warn!(event = "vpn_failover_failed", from = ?previous, error = %e,
                  "VPN failover не удался");
            self.enter_outage().await;
        } else {
            info!(event = "vpn_failover_completed", from = ?previous,
                  to = self.active_index.map(|i| self.upstreams[i].name()),
                  verification = "pre_activation_ping_only",
                  "аварийная активация upstream завершена");
        }
    }

    async fn try_recover(&mut self) -> Result<()> {
        for i in 0..self.upstreams.len() {
            let upstream = self.upstreams[i].clone();
            if let Err(e) = self.ensure_upstream_ready(i) {
                debug!(upstream = upstream.name(), error = %e, "не удалось запустить");
                continue;
            }
            if let Err(error) = upstream.health_check().await {
                debug!(event = "recovery_candidate_rejected", upstream = upstream.name(),
                       error = %error, "health-check кандидата не прошёл");
            } else {
                match self.activate(i).await {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        warn!(error = %e, "не удалось активировать upstream при восстановлении")
                    }
                }
            }
        }
        anyhow::bail!("нет здоровых upstream")
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        for task in self.recovery_tasks.iter().flatten() {
            task.abort();
        }
        for task in &mut self.recovery_tasks {
            if let Some(task) = task.take() {
                let _ = task.await;
            }
        }
        let mut results = Vec::new();
        if let Some(task) = self.health_task.take() {
            task.abort();
        }
        if let Some(task) = self.speed_task.take() {
            task.abort();
        }
        if let Some(task) = self.https_task.take() {
            task.abort();
        }
        if let Some(nat) = self.nat.take() {
            results.push(nat.cleanup().await.context("ошибка очистки NAT"));
        }
        if let Some(routing) = self.routing.take() {
            results.push(
                routing
                    .cleanup()
                    .await
                    .context("ошибка очистки маршрутизации"),
            );
        }
        for upstream in &self.upstreams {
            results.push(
                upstream
                    .stop()
                    .await
                    .with_context(|| format!("ошибка остановки {}", upstream.name())),
            );
        }
        crate::util::cleanup_result(results)
    }
}

fn speed_candidate(speeds: &[Option<f64>], active: usize, minimum: f64) -> Option<usize> {
    let current = speeds.get(active).copied().flatten()?;
    if minimum <= 0.0 || current >= minimum {
        return None;
    }
    speeds
        .iter()
        .enumerate()
        .filter_map(|(i, speed)| {
            speed
                .filter(|s| s.is_finite() && *s > current)
                .map(|s| (i, s))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::speed_candidate;

    #[test]
    fn speed_switch_requires_low_active_speed_and_faster_candidate() {
        let speeds = [Some(2.0), Some(10.0), None];
        assert_eq!(speed_candidate(&speeds, 0, 5.0), Some(1));
        assert_eq!(speed_candidate(&speeds, 0, 0.0), None);
        assert_eq!(speed_candidate(&speeds, 1, 5.0), None);
        assert_eq!(speed_candidate(&speeds, 2, 5.0), None);
    }
}
