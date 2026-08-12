use fractal_protocol::catalog::OptionRead;
use fractal_protocol::config::Value;
use fractal_protocol::diff::SemanticDiff;
use fractal_protocol::generations::{Generation, Kind, Outcome};
use fractal_protocol::messages::{Response, StagedChange};

pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

pub fn response(answer: &Response) -> Option<String> {
    Some(match answer {
        Response::Pong => "The agent is running.".to_string(),
        Response::Ok => return None,
        Response::Generations { generations } => history(generations),
        Response::Current { generation } => match generation {
            Some(g) => generation_line(g),
            None => "Nothing has been activated yet.".to_string(),
        },
        Response::Catalog { entries } => entries
            .iter()
            .map(|e| match &e.meta {
                Some(m) => match &m.type_name {
                    Some(t) => format!("{}  ({t})", e.key),
                    None => e.key.clone(),
                },
                None => e.key.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Response::OptionValue(read) => option(read),
        Response::StagedDiff { changes, .. } => staged(changes),
        Response::Committed { commit } => match commit {
            Some(hash) => format!("Committed {}.", short(hash)),
            None => "Nothing staged.".to_string(),
        },
        Response::Built { store_path, config_commit } => {
            format!("Built {store_path}\nfrom {}", short(config_commit))
        }
        Response::Diff(diff) => semantic(diff),
        Response::Evidence(evidence) => {
            let mut out = generation_line(&evidence.generation);
            if let Some(change) = &evidence.change {
                out.push_str("\n\n");
                out.push_str(&semantic(change));
            }
            out
        }
        Response::Activated { generation } => {
            format!("Activated.\n{}", generation_line(generation))
        }
        Response::Challenge(_) => return None,
        Response::Progress { line } => line.clone(),
        Response::Error { message } => format!("error: {message}"),
    })
}

pub fn staged(changes: &[StagedChange]) -> String {
    if changes.is_empty() {
        return "Nothing staged.".to_string();
    }
    changes
        .iter()
        .map(|staged| {
            let who = match staged.staged_by {
                Some(uid) => format!(" (uid {uid})"),
                None => String::new(),
            };
            format!("{}{who}", change_line(&staged.change.key, &staged.change.before, &staged.change.after))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn semantic(diff: &SemanticDiff) -> String {
    let mut out = Vec::new();
    if diff.options.is_empty() {
        out.push("No option changed.".to_string());
    } else {
        out.push("Options:".to_string());
        for change in &diff.options {
            out.push(format!("  {}", change_line(&change.key, &change.before, &change.after)));
        }
    }

    let delta: i64 = diff.closure.packages.values().map(|p| p.size_delta).sum();
    if !diff.closure.packages.is_empty() {
        out.push(format!(
            "\n{} package(s) differ, {} overall.",
            diff.closure.packages.len(),
            size(delta)
        ));
    }
    out.join("\n")
}

fn option(read: &OptionRead) -> String {
    let mut out = vec![read.key.clone()];
    let layer = |name: &str, value: Option<String>| value.map(|v| format!("  {name:<10}{v}"));
    out.extend(layer("staged", read.staged.as_ref().map(value)));
    out.extend(layer("effective", read.effective.as_ref().map(|s| value(&s.value))));
    out.extend(layer("declared", read.declared.as_ref().map(|s| value(&s.value))));
    if out.len() == 1 {
        out.push("  not set".to_string());
    }
    out.join("\n")
}

fn history(generations: &[Generation]) -> String {
    if generations.is_empty() {
        return "No generations yet.".to_string();
    }
    generations.iter().map(generation_line).collect::<Vec<_>>().join("\n")
}

fn generation_line(g: &Generation) -> String {
    let outcome = match &g.outcome {
        Outcome::Success => String::new(),
        Outcome::Failed { detail } => format!("  FAILED: {detail}"),
    };
    let kind = match g.kind {
        Kind::Rollback => " (rollback)",
        Kind::Forward => "",
    };
    format!(
        "{:>4}  {}  {}{kind}  by {}{outcome}",
        g.id,
        g.timestamp.strftime("%Y-%m-%d %H:%M"),
        short(&g.config_commit),
        g.actor,
    )
}

fn change_line(key: &str, before: &Option<Value>, after: &Option<Value>) -> String {
    match (before, after) {
        (None, Some(a)) => format!("{key} = {}", value(a)),
        (Some(b), None) => format!("{key}: {} removed", value(b)),
        (Some(b), Some(a)) => format!("{key}: {} -> {}", value(b), value(a)),
        (None, None) => key.to_string(),
    }
}

fn value(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "?".to_string())
}

fn size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let sign = if bytes < 0 { "-" } else { "+" };
    let mut n = bytes.unsigned_abs() as f64;
    let mut unit = 0;
    while n >= 1024.0 && unit < UNITS.len() - 1 {
        n /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{sign}{n:.0} {}", UNITS[unit])
    } else {
        format!("{sign}{n:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_deltas() {
        assert_eq!(size(0), "+0 B");
        assert_eq!(size(512), "+512 B");
        assert_eq!(size(-1536), "-1.5 KiB");
        assert_eq!(size(3 * 1024 * 1024), "+3.0 MiB");
    }

    #[test]
    fn a_change_reads_as_a_change() {
        let int = |n| Some(Value::Int(n));
        assert_eq!(change_line("a", &None, &int(1)), "a = 1");
        assert_eq!(change_line("a", &int(1), &None), "a: 1 removed");
        assert_eq!(change_line("a", &int(1), &int(2)), "a: 1 -> 2");
    }
}
