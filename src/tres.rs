//! Slurm's trackable-resource lists, as `squeue --Format=tres-alloc` and
//! `sacct --format=AllocTRES` both report them: `cpu=96,mem=4800G,node=3,
//! billing=96,gres/gpu=24`. Both are totals across the job's nodes, unlike
//! squeue's older `%b` and `%m`, which are per node.

/// Pull one value out of a TRES list.
pub fn value<'a>(spec: &'a str, key: &str) -> Option<&'a str> {
    spec.split(',')
        .filter_map(|entry| entry.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

/// A job is allocated memory in whatever unit it asked in, so put them all in
/// gigabytes and let the column compare like with like.
pub fn gigabytes(mem: &str) -> String {
    let Some((amount, unit)) = mem.split_at_checked(mem.len().saturating_sub(1)) else {
        return mem.to_string();
    };
    let Ok(amount) = amount.parse::<f64>() else {
        return mem.to_string();
    };

    let gb = match unit {
        "K" => amount / (1024.0 * 1024.0),
        "M" => amount / 1024.0,
        "G" => amount,
        "T" => amount * 1024.0,
        _ => return mem.to_string(),
    };

    format!("{}G", gb.round())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = "cpu=96,mem=4800G,node=3,billing=96,gres/gpu=24";

    #[test]
    fn reads_a_value_by_key() {
        assert_eq!(value(SPEC, "cpu"), Some("96"));
        assert_eq!(value(SPEC, "gres/gpu"), Some("24"));
    }

    /// A job that asked for no GPUs has no gres/gpu entry at all, rather than
    /// a zero.
    #[test]
    fn reports_a_missing_key() {
        assert_eq!(value("cpu=2,mem=16G,node=1", "gres/gpu"), None);
    }

    #[test]
    fn converts_memory_to_gigabytes() {
        assert_eq!(gigabytes("90000M"), "88G");
        assert_eq!(gigabytes("200G"), "200G");
        assert_eq!(gigabytes("2T"), "2048G");
        // Anything it does not recognise passes through rather than vanishing.
        assert_eq!(gigabytes("unknown"), "unknown");
        assert_eq!(gigabytes(""), "");
    }
}
