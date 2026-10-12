use super::*;
use crate::database::resources::TestClock;
use std::time::{Duration, Instant};

#[test]
fn deadline_expires_between_distinct_rows_without_timing_preparation() {
    // This deadline is already past in real time. Only the controlled clock
    // makes preparation and first-row success possible, regardless of load.
    let start = Instant::now() - Duration::from_secs(60);
    let clock = TestClock::at(start);
    let mut db = Database::in_memory().unwrap();
    let query = vec!["RETURN 1 AS x"; 100].join(" UNION ");
    let options = QueryOptions {
        deadline: Some(start + Duration::from_secs(1)),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(&query, &Parameters::new(), &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
    assert!(!cursor.connection.is_autocommit());
    clock.advance(Duration::from_secs(1));
    assert!(matches!(cursor.next(), Some(Err(Error::DeadlineExceeded))));
    assert!(cursor.next().is_none());
    assert!(cursor.next().is_none());
    assert!(cursor.connection.is_autocommit());
    drop(cursor);
    assert!(matches!(read.cursor(), Err(Error::DeadlineExceeded)));
    drop(read);
    assert!(matches!(
        db.prepare_read_with_options(&query, &Parameters::new(), &options),
        Err(Error::DeadlineExceeded)
    ));
    db.execute("CREATE (:AfterDeadline)").unwrap();
}
