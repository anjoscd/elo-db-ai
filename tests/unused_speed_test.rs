// Keep measurement isolated from tests competing for CPU with child processes.

#[test]
fn unused_query_with_22000_deliveries() {
    let dir = tempfile::tempdir().unwrap();
    let store = elo_ctx::store::Store::create(dir.path()).unwrap();
    store.conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<22000)
        INSERT INTO injections SELECT 's',x,'r' || (x % 100),'full','rule','global',1000000 FROM n;").unwrap();
    let mut samples = Vec::new();
    for _ in 0..30 {
        let start = std::time::Instant::now();
        let windows = store.unused_windows(1000001, 14);
        samples.push(start.elapsed());
        assert_eq!(windows.len(), 100);
        assert!(windows.values().all(|w| w.full_events == 220));
    }
    samples.sort();
    eprintln!(
        "unused_windows: 30 samples, 22000 deliveries, p95={:?}",
        samples[28]
    );
    let budget_ms = if cfg!(debug_assertions) { 250 } else { 50 };
    assert!(samples[28] < std::time::Duration::from_millis(budget_ms));
}
