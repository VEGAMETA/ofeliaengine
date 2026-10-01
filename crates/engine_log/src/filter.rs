//! Per-target level filters: `info,engine_render=debug,engine_rhi::fault=trace`.

use log::LevelFilter;

/// Which levels pass for which targets.
///
/// A spec is a comma-separated list of directives. `LEVEL` sets the default; `TARGET=LEVEL`
/// sets the level of a target and every target under it (`engine_render` covers
/// `engine_render::graph`); a bare `TARGET` enables everything for it. The longest
/// matching target wins. Levels: `off`, `error`, `warn`, `info`, `debug`, `trace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    default: LevelFilter,
    /// Longest target first, so the first match is the most specific.
    targets: Vec<(Box<str>, LevelFilter)>,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            default: LevelFilter::Info,
            targets: Vec::new(),
        }
    }
}

fn parse_level(text: &str) -> Option<LevelFilter> {
    Some(match text.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" | "all" => LevelFilter::Trace,
        _ => return None,
    })
}

impl Filter {
    /// Parses a spec (see the type's documentation). Empty directives are ignored.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut filter = Self::default();
        for directive in spec.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            match directive.split_once('=') {
                Some((target, level)) => {
                    let level = parse_level(level)
                        .ok_or_else(|| format!("unknown level in `{directive}`"))?;
                    let target = target.trim();
                    if target.is_empty() {
                        return Err(format!("no target in `{directive}`"));
                    }
                    filter.set(target, level);
                }
                None => match parse_level(directive) {
                    Some(level) => filter.default = level,
                    None => filter.set(directive, LevelFilter::Trace),
                },
            }
        }
        Ok(filter)
    }

    fn set(&mut self, target: &str, level: LevelFilter) {
        self.targets.retain(|(t, _)| &**t != target);
        self.targets.push((target.into(), level));
        self.targets
            .sort_by_key(|(t, _)| core::cmp::Reverse(t.len()));
    }

    /// The most verbose level for `target`.
    #[inline]
    pub fn level_for(&self, target: &str) -> LevelFilter {
        for (prefix, level) in &self.targets {
            if let Some(rest) = target.strip_prefix(&**prefix)
                && (rest.is_empty() || rest.starts_with("::"))
            {
                return *level;
            }
        }
        self.default
    }

    /// The most verbose level any target gets: what `log::set_max_level` should be, so
    /// the macros reject everything else with one comparison.
    pub fn max(&self) -> LevelFilter {
        self.targets
            .iter()
            .map(|(_, l)| *l)
            .fold(self.default, Ord::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_target_wins() {
        let f = Filter::parse("warn, engine_render=debug ,engine_render::graph=trace,naga=off")
            .unwrap();
        assert_eq!(f.level_for("engine_rhi::device"), LevelFilter::Warn);
        assert_eq!(f.level_for("engine_render"), LevelFilter::Debug);
        assert_eq!(f.level_for("engine_render::post"), LevelFilter::Debug);
        assert_eq!(f.level_for("engine_render::graph"), LevelFilter::Trace);
        assert_eq!(
            f.level_for("engine_render::graph::timing"),
            LevelFilter::Trace
        );
        // A prefix only matches whole path segments.
        assert_eq!(f.level_for("engine_renderer"), LevelFilter::Warn);
        assert_eq!(f.level_for("naga::front"), LevelFilter::Off);
        assert_eq!(f.max(), LevelFilter::Trace);
    }

    #[test]
    fn bare_target_enables_everything_and_errors_are_reported() {
        let f = Filter::parse("vulkan").unwrap();
        assert_eq!(f.level_for("vulkan"), LevelFilter::Trace);
        assert_eq!(f.level_for("other"), LevelFilter::Info);
        assert!(Filter::parse("engine_rhi=loud").is_err());
        assert!(Filter::parse("=info").is_err());
        assert_eq!(Filter::parse("").unwrap(), Filter::default());
    }
}
