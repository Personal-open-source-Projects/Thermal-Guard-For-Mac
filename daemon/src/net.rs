//! Pure network-health scoring.
//!
//! The daemon *measures* the link and says what is wrong with it; it does not
//! silently rewrite TCP tunables or switch radios. macOS already auto-tunes its
//! socket buffers, and a blind `sysctl`/`networksetup` change is the fastest way
//! to break connectivity for no gain. What userspace can honestly provide is the
//! evidence: signal-to-noise, negotiated rate, latency to the router and loss.
//!
//! Every input is supplied by the caller, so the table is testable without a
//! network.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Down,
    Wifi,
    Wired,
}

impl Link {
    pub fn as_str(&self) -> &'static str {
        match self {
            Link::Down => "down",
            Link::Wifi => "wifi",
            Link::Wired => "wired",
        }
    }
}

pub struct Inputs {
    pub link: Link,
    /// `None` when the radio cannot be read (wired, or wdutil unavailable).
    pub rssi_dbm: Option<f64>,
    pub noise_dbm: Option<f64>,
    pub tx_mbps: Option<f64>,
    pub gateway_ms: Option<f64>,
    pub loss_pct: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub score: u8,
    pub grade: &'static str,
    pub issues: Vec<&'static str>,
}

impl Report {
    pub fn note(&self) -> String {
        if self.issues.is_empty() {
            "Link is healthy.".to_string()
        } else {
            self.issues.join(" ")
        }
    }
}

/// Score 0–100 from measurable link facts, never from a guess. Wired is scored
/// higher only because it has no RF variables at all.
pub fn evaluate(i: &Inputs) -> Report {
    if i.link == Link::Down {
        return Report {
            score: 0,
            grade: "offline",
            issues: vec!["No active default route."],
        };
    }

    let mut score: f64 = if i.link == Link::Wifi { 60.0 } else { 75.0 };
    let mut issues = Vec::new();

    if i.link == Link::Wifi {
        match i.rssi_dbm {
            Some(rssi) if rssi >= -50.0 => score += 25.0,
            Some(rssi) if rssi >= -60.0 => score += 18.0,
            Some(rssi) if rssi >= -70.0 => score += 8.0,
            Some(rssi) if rssi >= -80.0 => {
                issues.push("Wi-Fi signal is weak; move closer or use 5 GHz.");
            }
            Some(_) => {
                score -= 15.0;
                issues.push("Wi-Fi signal is very weak; the link will stall.");
            }
            None => {}
        }
        if let (Some(rssi), Some(noise)) = (i.rssi_dbm, i.noise_dbm) {
            if rssi - noise < 20.0 {
                score -= 10.0;
                issues.push("Noisy channel; pick a less crowded channel on the router.");
            }
        }
        match i.tx_mbps {
            Some(rate) if rate >= 300.0 => score += 15.0,
            Some(rate) if rate >= 100.0 => score += 8.0,
            Some(rate) if rate < 30.0 => {
                score -= 5.0;
                issues.push("Negotiated Wi-Fi rate is low; 5 GHz would be faster.");
            }
            _ => {}
        }
    }

    match i.gateway_ms {
        Some(ms) if ms <= 3.0 => score += 10.0,
        Some(ms) if ms <= 10.0 => score += 6.0,
        Some(ms) if ms <= 30.0 => score += 2.0,
        Some(_) => {
            score -= 8.0;
            issues.push("High latency to the router; other devices may be saturating it.");
        }
        None => {}
    }

    let loss = if i.loss_pct.is_finite() {
        i.loss_pct.clamp(0.0, 100.0)
    } else {
        0.0
    };
    if loss > 5.0 {
        score -= 25.0;
        issues.push("Packet loss above 5 %: the link is dropping traffic.");
    } else if loss > 1.0 {
        score -= 10.0;
        issues.push("Small but real packet loss on the link.");
    } else if loss > 0.0 {
        score -= 3.0;
    }

    let score = score.clamp(0.0, 100.0).round() as u8;
    let grade = match score {
        85..=100 => "excellent",
        70..=84 => "good",
        50..=69 => "fair",
        _ => "poor",
    };
    Report {
        score,
        grade,
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wifi() -> Inputs {
        Inputs {
            link: Link::Wifi,
            rssi_dbm: Some(-45.0),
            noise_dbm: Some(-95.0),
            tx_mbps: Some(867.0),
            gateway_ms: Some(2.0),
            loss_pct: 0.0,
        }
    }

    #[test]
    fn a_strong_idle_link_scores_excellent() {
        let r = evaluate(&wifi());
        assert!(r.score >= 85, "score {} not excellent", r.score);
        assert_eq!(r.grade, "excellent");
        assert!(r.issues.is_empty());
        assert_eq!(r.note(), "Link is healthy.");
    }

    #[test]
    fn offline_is_zero_and_never_pretends_otherwise() {
        let r = evaluate(&Inputs {
            link: Link::Down,
            rssi_dbm: None,
            noise_dbm: None,
            tx_mbps: None,
            gateway_ms: None,
            loss_pct: 0.0,
        });
        assert_eq!(r.score, 0);
        assert_eq!(r.grade, "offline");
    }

    #[test]
    fn weak_noisy_wifi_is_flagged_with_actions() {
        let r = evaluate(&Inputs {
            rssi_dbm: Some(-76.0),
            noise_dbm: Some(-80.0),
            tx_mbps: Some(24.0),
            gateway_ms: Some(45.0),
            ..wifi()
        });
        assert!(r.score < 60, "score {} should be poor/fair", r.score);
        assert!(r.issues.iter().any(|i| i.contains("weak")));
        assert!(r.issues.iter().any(|i| i.contains("Noisy")));
        assert!(r.issues.iter().any(|i| i.contains("rate")));
    }

    #[test]
    fn loss_and_latency_are_penalised() {
        let clean = evaluate(&wifi()).score;
        let lossy = evaluate(&Inputs {
            loss_pct: 6.0,
            ..wifi()
        });
        assert!(lossy.score < clean);
        assert!(lossy.issues.iter().any(|i| i.contains("Packet loss")));

        let slow = evaluate(&Inputs {
            gateway_ms: Some(55.0),
            ..wifi()
        });
        assert!(slow.score < clean);
        assert!(slow.issues.iter().any(|i| i.contains("latency")));
    }

    #[test]
    fn wired_needs_no_radio_data() {
        let r = evaluate(&Inputs {
            link: Link::Wired,
            rssi_dbm: None,
            noise_dbm: None,
            tx_mbps: None,
            gateway_ms: Some(1.0),
            loss_pct: 0.0,
        });
        assert!(r.score >= 85);
        assert_eq!(r.grade, "excellent");
    }

    #[test]
    fn non_finite_inputs_never_produce_a_wild_score() {
        let r = evaluate(&Inputs {
            rssi_dbm: Some(f64::NAN),
            noise_dbm: Some(f64::NAN),
            tx_mbps: Some(f64::NAN),
            gateway_ms: Some(f64::NAN),
            loss_pct: f64::NAN,
            ..wifi()
        });
        assert!(r.score <= 100);
        assert!(!r.grade.is_empty());
    }
}
