//! Custom temporal systems: persistent world clocks with constant, piecewise,
//! paused (active-time), or table-lookup rates. Mirrors localSeconds() in the
//! CTCL Worker (src/worker.js) - same formulas, so a system defined once behaves
//! identically whether it's evaluated by the hosted API or this local core.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub until: Option<f64>,
    pub rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pause {
    pub from: f64,
    pub to: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TablePoint {
    pub parent: f64,
    pub local: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Rate {
    Constant { value: f64 },
    Piecewise { segments: Vec<Segment> },
    Paused { value: f64, pauses: Vec<Pause> },
    Table { table: Vec<TablePoint> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalSystem {
    pub id: String,
    /// epoch, in parent (unix) seconds
    pub epoch_parent_sec: f64,
    pub rate: Rate,
    #[serde(default)]
    pub offset: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalTimeExtra {
    pub wall_elapsed_s: Option<f64>,
    pub paused_elapsed_s: Option<f64>,
    pub active_elapsed_s: Option<f64>,
    pub currently_paused: Option<bool>,
}

impl TemporalSystem {
    /// Given a parent-timeline instant (unix seconds), compute this system's
    /// local time in seconds, plus rate-type-specific extras (active-time for
    /// paused systems, wall-elapsed for the rest).
    pub fn local_seconds(&self, parent_sec: f64) -> (f64, LocalTimeExtra) {
        let elapsed = parent_sec - self.epoch_parent_sec;
        match &self.rate {
            Rate::Constant { value } => (
                value * elapsed + self.offset,
                LocalTimeExtra {
                    wall_elapsed_s: Some(elapsed),
                    ..Default::default()
                },
            ),
            Rate::Paused { value, pauses } => {
                let mut paused = 0.0;
                let mut currently_paused = false;
                for pz in pauses {
                    let pt = pz.to.unwrap_or(f64::INFINITY);
                    let lo = pz.from.max(self.epoch_parent_sec);
                    let hi = pt.min(parent_sec);
                    if hi > lo {
                        paused += hi - lo;
                    }
                    if parent_sec >= pz.from && parent_sec < pt {
                        currently_paused = true;
                    }
                }
                let active = elapsed - paused;
                (
                    active * value + self.offset,
                    LocalTimeExtra {
                        wall_elapsed_s: Some(elapsed),
                        paused_elapsed_s: Some(paused),
                        active_elapsed_s: Some(active),
                        currently_paused: Some(currently_paused),
                    },
                )
            }
            Rate::Piecewise { segments } => {
                let mut local = 0.0;
                let mut cursor = self.epoch_parent_sec;
                for seg in segments {
                    let until = seg.until.unwrap_or(parent_sec);
                    let hi = until.min(parent_sec);
                    if hi > cursor {
                        local += seg.rate * (hi - cursor);
                        cursor = hi;
                    }
                    if cursor >= parent_sec {
                        break;
                    }
                }
                if cursor < parent_sec {
                    if let Some(last) = segments.last() {
                        local += last.rate * (parent_sec - cursor);
                    }
                }
                (
                    local + self.offset,
                    LocalTimeExtra {
                        wall_elapsed_s: Some(elapsed),
                        ..Default::default()
                    },
                )
            }
            Rate::Table { table } => {
                if table.is_empty() {
                    return (self.offset, LocalTimeExtra::default());
                }
                let mut sorted = table.clone();
                sorted.sort_by(|a, b| a.parent.partial_cmp(&b.parent).unwrap());
                if parent_sec <= sorted[0].parent {
                    return (sorted[0].local + self.offset, LocalTimeExtra::default());
                }
                let last = sorted.last().unwrap();
                if parent_sec >= last.parent {
                    return (last.local + self.offset, LocalTimeExtra::default());
                }
                for w in sorted.windows(2) {
                    let (a, b) = (&w[0], &w[1]);
                    if parent_sec >= a.parent && parent_sec <= b.parent {
                        let f = if b.parent == a.parent {
                            0.0
                        } else {
                            (parent_sec - a.parent) / (b.parent - a.parent)
                        };
                        return (
                            a.local + f * (b.local - a.local) + self.offset,
                            LocalTimeExtra::default(),
                        );
                    }
                }
                (self.offset, LocalTimeExtra::default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_rate_scales_elapsed_time() {
        let sys = TemporalSystem {
            id: "user:game_world".into(),
            epoch_parent_sec: 1_700_000_000.0,
            rate: Rate::Constant { value: 20.0 },
            offset: 0.0,
        };
        let (local, extra) = sys.local_seconds(1_700_000_100.0);
        assert_eq!(local, 2000.0); // 100s elapsed * 20x
        assert_eq!(extra.wall_elapsed_s, Some(100.0));
    }

    #[test]
    fn paused_rate_excludes_pause_window_from_active_time() {
        let sys = TemporalSystem {
            id: "agent:a:life".into(),
            epoch_parent_sec: 1_700_000_000.0,
            rate: Rate::Paused {
                value: 1.0,
                pauses: vec![Pause { from: 1_700_003_600.0, to: Some(1_700_007_200.0) }],
            },
            offset: 0.0,
        };
        // inside the pause window
        let (_, extra) = sys.local_seconds(1_700_005_000.0);
        assert_eq!(extra.currently_paused, Some(true));
        // after the pause window: active time excludes the paused 3600s
        let (local, extra2) = sys.local_seconds(1_700_010_000.0);
        assert_eq!(extra2.currently_paused, Some(false));
        assert_eq!(extra2.paused_elapsed_s, Some(3600.0));
        assert_eq!(local, 10_000.0 - 3600.0);
    }

    #[test]
    fn piecewise_rate_integrates_segments() {
        let sys = TemporalSystem {
            id: "test:piecewise".into(),
            epoch_parent_sec: 1_700_000_000.0,
            rate: Rate::Piecewise {
                segments: vec![
                    Segment { until: Some(1_700_010_000.0), rate: 1.0 },
                    Segment { until: None, rate: 5.0 },
                ],
            },
            offset: 0.0,
        };
        // exactly at the boundary: 10000s at rate 1
        let (local, _) = sys.local_seconds(1_700_010_000.0);
        assert_eq!(local, 10_000.0);
        // 100s past the boundary at rate 5
        let (local2, _) = sys.local_seconds(1_700_010_100.0);
        assert_eq!(local2, 10_000.0 + 100.0 * 5.0);
    }

    #[test]
    fn table_rate_interpolates_linearly() {
        let sys = TemporalSystem {
            id: "test:table".into(),
            epoch_parent_sec: 0.0,
            rate: Rate::Table {
                table: vec![
                    TablePoint { parent: 0.0, local: 0.0 },
                    TablePoint { parent: 100.0, local: 1000.0 },
                ],
            },
            offset: 0.0,
        };
        let (local, _) = sys.local_seconds(50.0);
        assert_eq!(local, 500.0); // halfway
    }
}
