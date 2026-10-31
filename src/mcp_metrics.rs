//! Local, metadata-only MCP tool timing. Kept outside the synced board folder.
//! One aggregate row per UTC day; no tool names, arguments, prompts or results.
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub days: Vec<Day>,
    pub total_calls: i64,
    pub fastest_us: Option<i64>,
    pub slowest_us: Option<i64>,
    pub average_us: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Day {
    pub day: String,
    pub calls: i64,
}

/// The value is measured before this write, so the telemetry itself is not part of tool latency.
pub fn record(path: &Path, elapsed: Duration) -> Result<()> {
    let parent = path.parent().expect("metrics file has a parent");
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let conn = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    conn.busy_timeout(Duration::from_millis(250))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS daily (
            day TEXT PRIMARY KEY,
            calls INTEGER NOT NULL,
            total_us INTEGER NOT NULL,
            fastest_us INTEGER NOT NULL,
            slowest_us INTEGER NOT NULL
        );",
    )?;
    let micros = elapsed.as_micros().max(1).min(i64::MAX as u128) as i64;
    conn.execute(
        "INSERT INTO daily (day, calls, total_us, fastest_us, slowest_us)
         VALUES (date('now'), 1, ?1, ?1, ?1)
         ON CONFLICT(day) DO UPDATE SET
           calls = calls + 1, total_us = total_us + excluded.total_us,
           fastest_us = min(fastest_us, excluded.fastest_us),
           slowest_us = max(slowest_us, excluded.slowest_us)",
        params![micros],
    )?;
    Ok(())
}

/// All-time latency statistics; the seven UTC day buckets always include zero days.
pub fn snapshot(path: &Path) -> Result<Summary> {
    match fs::metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Summary::default()),
        Err(error) => return Err(error.into()),
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(250))?;
    let (total_calls, fastest_us, slowest_us, total_us): (i64, Option<i64>, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT coalesce(sum(calls), 0), min(fastest_us), max(slowest_us), sum(total_us) FROM daily",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let days = conn
        .prepare(
            "WITH RECURSIVE dates(day) AS (
            SELECT date('now', '-6 days') UNION ALL
            SELECT date(day, '+1 day') FROM dates WHERE day < date('now')
         ) SELECT dates.day, coalesce(daily.calls, 0) FROM dates
         LEFT JOIN daily ON daily.day = dates.day ORDER BY dates.day",
        )?
        .query_map([], |row| Ok(Day { day: row.get(0)?, calls: row.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Summary {
        days,
        total_calls,
        fastest_us,
        slowest_us,
        average_us: total_us.filter(|_| total_calls > 0).map(|value| value as f64 / total_calls as f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_daily_aggregates_and_computes_weighted_average() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/metrics.sqlite");
        assert_eq!(snapshot(&path).unwrap().total_calls, 0);
        record(&path, Duration::from_micros(300)).unwrap();
        record(&path, Duration::from_micros(900)).unwrap();
        record(&path, Duration::from_micros(600)).unwrap();
        let stats = snapshot(&path).unwrap();
        assert_eq!(stats.total_calls, 3);
        assert_eq!(stats.fastest_us, Some(300));
        assert_eq!(stats.slowest_us, Some(900));
        assert_eq!(stats.average_us, Some(600.0));
        assert_eq!(stats.days.len(), 7);
        assert_eq!(stats.days.last().unwrap().calls, 3);
        assert_eq!(stats.days.iter().map(|d| d.calls).sum::<i64>(), 3);
        let conn = Connection::open(&path).unwrap();
        let columns: String = conn
            .query_row("SELECT group_concat(name, ',') FROM pragma_table_info('daily')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(columns, "day,calls,total_us,fastest_us,slowest_us");
        conn.execute("INSERT INTO daily VALUES (date('now', '-8 days'), 2, 4000, 1000, 3000)", []).unwrap();
        let stats = snapshot(&path).unwrap();
        assert_eq!(stats.total_calls, 5);
        assert_eq!(stats.fastest_us, Some(300));
        assert_eq!(stats.slowest_us, Some(3000));
        assert_eq!(stats.average_us, Some(1160.0));
        assert_eq!(stats.days.iter().map(|d| d.calls).sum::<i64>(), 3);
    }

    #[test]
    fn broken_metrics_file_is_reported_instead_of_faking_zero_calls() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("broken.sqlite");
        fs::write(&path, "not SQLite").unwrap();
        assert!(snapshot(&path).is_err());
    }
}
