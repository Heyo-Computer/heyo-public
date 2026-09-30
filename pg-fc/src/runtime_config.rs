//! The configuration knobs that can change while the pooler runs.
//!
//! Everything else in [`Config`] is read once from the environment. These few
//! are the ones an operator reaches for in the middle of an incident or a
//! capacity change — how long a VM stays warm, how many spares to keep, when
//! the offload ladder fires — and a restart to change them drops every client
//! session on the host. So they live here, behind a lock the loops that use
//! them read on every pass, and `PUT /api/config` swaps them.
//!
//! ## What can and cannot change
//!
//! A knob is mutable only if the subsystem that reads it was switched on at
//! boot. Turning idle reaping, a tier or the spare pool *on* means spawning a
//! loop (and for tiers, directories and credentials that are env-only), which
//! is a restart's job; the `GET` says so per knob rather than accepting a value
//! nothing will read.
//!
//! ## Persistence
//!
//! Overrides are written to `runtime-config.json` beside the registry file and
//! laid over the environment at boot, so a change survives a restart — and the
//! `GET` reports each knob's source, so "why is this not what the env file
//! says" has an answer on the page.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use pg_fc_api::{ConfigView, KnobInfo, KnobSource, RuntimeKnobs};
use tracing::{info, warn};

use crate::config::Config;

/// Shortest idle timeout accepted at runtime. Lower than this and the reaper
/// stops VMs faster than a client can reconnect between queries.
const MIN_IDLE_SECS: u64 = 10;
/// Shortest offload threshold accepted at runtime.
const MIN_TIER_SECS: u64 = 60;

/// Which subsystems were on at boot — the ones whose knobs may change.
#[derive(Debug, Clone, Copy, Default)]
struct Enabled {
    idle: bool,
    spares: bool,
    compact: bool,
    freeze: bool,
    archive: bool,
}

pub struct RuntimeConfig {
    path: PathBuf,
    enabled: Enabled,
    /// The values the environment (or its defaults) gave at boot.
    base: RuntimeKnobs,
    /// Which env vars were actually set, for provenance.
    env_set: Vec<&'static str>,
    /// Persisted overrides, laid over `base`.
    overrides: RwLock<RuntimeKnobs>,
    /// Env-only settings reported read-only by the `GET`.
    fixed: Vec<KnobInfo>,
}

const IDLE: &str = "PG_VM_POOL_IDLE_TIMEOUT_SECS";
const IDLE_FAST: &str = "PG_VM_POOL_IDLE_TIMEOUT_FAST_SECS";
const SPARES: &str = "PG_VM_POOL_WARM_SPARES";
const COMPACT: &str = "PG_VM_POOL_COMPACT_AFTER_SECS";
const FREEZE: &str = "PG_VM_POOL_FREEZE_AFTER_SECS";
const ARCHIVE: &str = "PG_VM_POOL_ARCHIVE_AFTER_SECS";

impl RuntimeConfig {
    /// Build from the boot config and load any persisted overrides from
    /// `runtime-config.json` beside `cfg.state_file`.
    pub fn load(cfg: &Config) -> Self {
        let path = cfg
            .state_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("runtime-config.json");
        let rc = Self::from_config(cfg, path);
        match std::fs::read(&rc.path) {
            Ok(bytes) => match serde_json::from_slice::<RuntimeKnobs>(&bytes) {
                Ok(saved) => {
                    let kept = rc.admissible(saved);
                    if kept != RuntimeKnobs::default() {
                        info!(
                            "runtime config: applying persisted overrides from {}",
                            rc.path.display()
                        );
                    }
                    *rc.overrides.write().unwrap() = kept;
                }
                Err(e) => warn!(
                    "runtime config: ignoring unreadable {} ({e}); the environment's values apply",
                    rc.path.display()
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("runtime config: cannot read {}: {e}", rc.path.display()),
        }
        rc
    }

    fn from_config(cfg: &Config, path: PathBuf) -> Self {
        let enabled = Enabled {
            idle: cfg.idle_timeout.is_some(),
            spares: cfg.warm_spares > 0,
            compact: cfg.compact.is_some(),
            freeze: cfg.freeze.is_some(),
            archive: cfg.archive.is_some(),
        };
        let base = RuntimeKnobs {
            idle_timeout_secs: cfg.idle_timeout.map(|d| d.as_secs()),
            idle_timeout_fast_secs: enabled
                .idle
                .then(|| cfg.idle_timeout_fast.map(|d| d.as_secs()).unwrap_or(0)),
            warm_spares: enabled
                .spares
                .then(|| cfg.warm_spares.min(crate::spares::MAX_SPARES)),
            compact_after_secs: cfg.compact.as_ref().map(|c| c.compact_after.as_secs()),
            freeze_after_secs: cfg.freeze.as_ref().map(|f| f.freeze_after.as_secs()),
            archive_after_secs: cfg.archive.as_ref().map(|a| a.archive_after.as_secs()),
        };
        let env_set = [IDLE, IDLE_FAST, SPARES, COMPACT, FREEZE, ARCHIVE]
            .into_iter()
            .filter(|k| std::env::var_os(k).is_some())
            .collect();
        let fixed_knob = |key: &str, env: &str, value: Option<String>| KnobInfo {
            key: key.to_string(),
            env: env.to_string(),
            value,
            source: if std::env::var_os(env).is_some() {
                KnobSource::Env
            } else {
                KnobSource::Default
            },
            mutable: false,
            note: Some("read at boot; change the environment and restart".into()),
        };
        let fixed = vec![
            fixed_knob(
                "listen",
                "PG_VM_POOL_LISTEN",
                Some(cfg.listen_addr.to_string()),
            ),
            fixed_knob("image", "PG_VM_POOL_IMAGE", Some(cfg.image.clone())),
            fixed_knob(
                "size_class",
                "PG_VM_POOL_SIZE_CLASS",
                Some(cfg.size_class.as_str().to_string()),
            ),
            fixed_knob(
                "keepalive_schemas",
                "PG_VM_POOL_KEEPALIVE_SCHEMAS",
                Some({
                    let mut v: Vec<&str> =
                        cfg.keepalive_schemas.iter().map(String::as_str).collect();
                    v.sort_unstable();
                    v.join(",")
                }),
            ),
            fixed_knob(
                "idle_drain_window_secs",
                "PG_VM_POOL_IDLE_DRAIN_WINDOW_SECS",
                cfg.idle_drain_window.map(|d| d.as_secs().to_string()),
            ),
            fixed_knob(
                "chilled_vehicles",
                "PG_VM_POOL_CHILLED_VEHICLES",
                Some(cfg.chilled_vehicles.to_string()),
            ),
        ];
        Self {
            path,
            enabled,
            base,
            env_set,
            overrides: RwLock::new(RuntimeKnobs::default()),
            fixed,
        }
    }

    /// Drop the parts of `k` whose subsystem is off, with a warning — a
    /// persisted override for a tier that has since been switched off in the
    /// environment must not resurrect it.
    fn admissible(&self, mut k: RuntimeKnobs) -> RuntimeKnobs {
        let e = self.enabled;
        let drop_if = |off: bool, v: &mut Option<u64>, name: &str| {
            if off && v.take().is_some() {
                warn!("runtime config: ignoring saved {name}: that subsystem is off in this boot");
            }
        };
        drop_if(!e.idle, &mut k.idle_timeout_secs, "idle_timeout_secs");
        drop_if(
            !e.idle,
            &mut k.idle_timeout_fast_secs,
            "idle_timeout_fast_secs",
        );
        drop_if(!e.compact, &mut k.compact_after_secs, "compact_after_secs");
        drop_if(!e.freeze, &mut k.freeze_after_secs, "freeze_after_secs");
        drop_if(!e.archive, &mut k.archive_after_secs, "archive_after_secs");
        if !e.spares && k.warm_spares.take().is_some() {
            warn!("runtime config: ignoring saved warm_spares: the spare pool is off in this boot");
        }
        k
    }

    /// The values in force: overrides over the boot values.
    pub fn effective(&self) -> RuntimeKnobs {
        let o = self.overrides.read().unwrap();
        let b = &self.base;
        RuntimeKnobs {
            idle_timeout_secs: o.idle_timeout_secs.or(b.idle_timeout_secs),
            idle_timeout_fast_secs: o.idle_timeout_fast_secs.or(b.idle_timeout_fast_secs),
            warm_spares: o.warm_spares.or(b.warm_spares),
            compact_after_secs: o.compact_after_secs.or(b.compact_after_secs),
            freeze_after_secs: o.freeze_after_secs.or(b.freeze_after_secs),
            archive_after_secs: o.archive_after_secs.or(b.archive_after_secs),
        }
    }

    pub fn idle_timeout(&self) -> Option<Duration> {
        self.effective().idle_timeout_secs.map(Duration::from_secs)
    }

    /// `None` when the two-speed reaper is off. Clamped to the normal timeout,
    /// as at boot: it may only pull a stop earlier.
    pub fn idle_timeout_fast(&self) -> Option<Duration> {
        let k = self.effective();
        let fast = k.idle_timeout_fast_secs.filter(|&s| s > 0)?;
        Some(Duration::from_secs(match k.idle_timeout_secs {
            Some(normal) => fast.min(normal),
            None => fast,
        }))
    }

    pub fn warm_spares(&self) -> Option<usize> {
        self.effective().warm_spares
    }

    pub fn compact_after(&self) -> Option<Duration> {
        self.effective().compact_after_secs.map(Duration::from_secs)
    }

    pub fn freeze_after(&self) -> Option<Duration> {
        self.effective().freeze_after_secs.map(Duration::from_secs)
    }

    pub fn archive_after(&self) -> Option<Duration> {
        self.effective().archive_after_secs.map(Duration::from_secs)
    }

    /// Validate `patch` (absent fields are left alone), persist the merged
    /// overrides, and swap them in. Returns the new effective values.
    pub fn apply(&self, patch: RuntimeKnobs) -> Result<RuntimeKnobs, String> {
        let e = self.enabled;
        let off = |on: bool, set: bool, name: &str, env: &str| -> Result<(), String> {
            if set && !on {
                Err(format!(
                    "{name} cannot be changed: {env} was off at boot, and switching it on needs a restart"
                ))
            } else {
                Ok(())
            }
        };
        off(
            e.idle,
            patch.idle_timeout_secs.is_some(),
            "idle_timeout_secs",
            IDLE,
        )?;
        off(
            e.idle,
            patch.idle_timeout_fast_secs.is_some(),
            "idle_timeout_fast_secs",
            IDLE,
        )?;
        off(e.spares, patch.warm_spares.is_some(), "warm_spares", SPARES)?;
        off(
            e.compact,
            patch.compact_after_secs.is_some(),
            "compact_after_secs",
            COMPACT,
        )?;
        off(
            e.freeze,
            patch.freeze_after_secs.is_some(),
            "freeze_after_secs",
            FREEZE,
        )?;
        off(
            e.archive,
            patch.archive_after_secs.is_some(),
            "archive_after_secs",
            ARCHIVE,
        )?;

        if let Some(s) = patch.idle_timeout_secs
            && s < MIN_IDLE_SECS
        {
            return Err(format!(
                "idle_timeout_secs must be at least {MIN_IDLE_SECS}"
            ));
        }
        if let Some(n) = patch.warm_spares
            && n > crate::spares::MAX_SPARES
        {
            return Err(format!(
                "warm_spares may be at most {}",
                crate::spares::MAX_SPARES
            ));
        }
        for (name, v) in [
            ("compact_after_secs", patch.compact_after_secs),
            ("freeze_after_secs", patch.freeze_after_secs),
            ("archive_after_secs", patch.archive_after_secs),
        ] {
            if let Some(s) = v
                && s < MIN_TIER_SECS
            {
                return Err(format!("{name} must be at least {MIN_TIER_SECS}"));
            }
        }

        let mut next = self.overrides.read().unwrap().clone();
        macro_rules! merge {
            ($($f:ident),*) => { $( if patch.$f.is_some() { next.$f = patch.$f; } )* };
        }
        merge!(
            idle_timeout_secs,
            idle_timeout_fast_secs,
            warm_spares,
            compact_after_secs,
            freeze_after_secs,
            archive_after_secs
        );
        self.persist(&next)
            .map_err(|e| format!("could not save {}: {e}", self.path.display()))?;
        *self.overrides.write().unwrap() = next;
        let eff = self.effective();
        info!("runtime config changed: {eff:?}");
        Ok(eff)
    }

    /// Write-then-rename, so a crash mid-write keeps the previous overrides.
    fn persist(&self, k: &RuntimeKnobs) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(k).map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// `GET /api/config`.
    pub fn view(&self) -> ConfigView {
        let o = self.overrides.read().unwrap().clone();
        let eff = self.effective();
        let e = self.enabled;
        let knob = |key: &str,
                    env: &'static str,
                    on: bool,
                    value: Option<String>,
                    overridden: bool| KnobInfo {
            key: key.to_string(),
            env: env.to_string(),
            value,
            source: if overridden {
                KnobSource::Override
            } else if self.env_set.contains(&env) {
                KnobSource::Env
            } else {
                KnobSource::Default
            },
            mutable: on,
            note: (!on).then(|| format!("{env} was off at boot; switching it on needs a restart")),
        };
        let s = |v: Option<u64>| v.map(|n| n.to_string());
        let mut knobs = vec![
            knob(
                "idle_timeout_secs",
                IDLE,
                e.idle,
                s(eff.idle_timeout_secs),
                o.idle_timeout_secs.is_some(),
            ),
            knob(
                "idle_timeout_fast_secs",
                IDLE_FAST,
                e.idle,
                s(eff.idle_timeout_fast_secs),
                o.idle_timeout_fast_secs.is_some(),
            ),
            knob(
                "warm_spares",
                SPARES,
                e.spares,
                eff.warm_spares.map(|n| n.to_string()),
                o.warm_spares.is_some(),
            ),
            knob(
                "compact_after_secs",
                COMPACT,
                e.compact,
                s(eff.compact_after_secs),
                o.compact_after_secs.is_some(),
            ),
            knob(
                "freeze_after_secs",
                FREEZE,
                e.freeze,
                s(eff.freeze_after_secs),
                o.freeze_after_secs.is_some(),
            ),
            knob(
                "archive_after_secs",
                ARCHIVE,
                e.archive,
                s(eff.archive_after_secs),
                o.archive_after_secs.is_some(),
            ),
        ];
        knobs.extend(self.fixed.iter().cloned());
        ConfigView {
            effective: eff,
            knobs,
        }
    }
}

#[cfg(test)]
impl RuntimeConfig {
    /// A config with every subsystem on and the given boot values, persisting
    /// to `path`.
    fn for_test(path: PathBuf, base: RuntimeKnobs) -> Self {
        Self {
            path,
            enabled: Enabled {
                idle: true,
                spares: true,
                compact: true,
                freeze: true,
                archive: false,
            },
            base,
            env_set: vec![IDLE],
            overrides: RwLock::new(RuntimeKnobs::default()),
            fixed: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pgfc-runtime-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("runtime-config.json")
    }

    fn base() -> RuntimeKnobs {
        RuntimeKnobs {
            idle_timeout_secs: Some(900),
            idle_timeout_fast_secs: Some(60),
            warm_spares: Some(2),
            compact_after_secs: Some(3600),
            freeze_after_secs: Some(86_400),
            archive_after_secs: None,
        }
    }

    #[test]
    fn overrides_win_and_persist_and_absent_fields_are_left_alone() {
        let path = tmp("persist");
        let rc = RuntimeConfig::for_test(path.clone(), base());
        let eff = rc
            .apply(RuntimeKnobs {
                idle_timeout_secs: Some(300),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(eff.idle_timeout_secs, Some(300));
        assert_eq!(
            eff.warm_spares,
            Some(2),
            "untouched knobs keep their boot value"
        );
        rc.apply(RuntimeKnobs {
            warm_spares: Some(4),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            rc.idle_timeout(),
            Some(Duration::from_secs(300)),
            "a later patch keeps earlier overrides"
        );

        let saved: RuntimeKnobs = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            RuntimeKnobs {
                idle_timeout_secs: Some(300),
                warm_spares: Some(4),
                ..Default::default()
            }
        );

        let view = rc.view();
        let src = |k: &str| view.knobs.iter().find(|i| i.key == k).unwrap().source;
        assert_eq!(src("idle_timeout_secs"), KnobSource::Override);
        assert_eq!(src("compact_after_secs"), KnobSource::Default);
    }

    #[test]
    fn a_knob_for_a_subsystem_that_was_off_at_boot_is_refused() {
        let rc = RuntimeConfig::for_test(tmp("off"), base());
        let err = rc
            .apply(RuntimeKnobs {
                archive_after_secs: Some(604_800),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.contains("PG_VM_POOL_ARCHIVE_AFTER_SECS"), "{err}");
        let view = rc.view();
        let archive = view
            .knobs
            .iter()
            .find(|k| k.key == "archive_after_secs")
            .unwrap();
        assert!(!archive.mutable);
        assert!(archive.value.is_none());
    }

    #[test]
    fn nonsense_values_are_refused_before_anything_is_written() {
        let path = tmp("bad");
        let rc = RuntimeConfig::for_test(path.clone(), base());
        assert!(
            rc.apply(RuntimeKnobs {
                idle_timeout_secs: Some(1),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            rc.apply(RuntimeKnobs {
                warm_spares: Some(1000),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            rc.apply(RuntimeKnobs {
                freeze_after_secs: Some(5),
                ..Default::default()
            })
            .is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn the_fast_timeout_is_clamped_to_the_normal_one_and_zero_turns_it_off() {
        let rc = RuntimeConfig::for_test(tmp("fast"), base());
        rc.apply(RuntimeKnobs {
            idle_timeout_secs: Some(30),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(rc.idle_timeout_fast(), Some(Duration::from_secs(30)));
        rc.apply(RuntimeKnobs {
            idle_timeout_fast_secs: Some(0),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(rc.idle_timeout_fast(), None);
    }

    #[test]
    fn a_saved_override_for_a_disabled_subsystem_is_dropped_on_load() {
        let rc = RuntimeConfig::for_test(tmp("admit"), base());
        let kept = rc.admissible(RuntimeKnobs {
            archive_after_secs: Some(604_800),
            idle_timeout_secs: Some(120),
            ..Default::default()
        });
        assert_eq!(
            kept,
            RuntimeKnobs {
                idle_timeout_secs: Some(120),
                ..Default::default()
            }
        );
    }
}
