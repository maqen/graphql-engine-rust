use std::sync::atomic::AtomicUsize;

use hge_core::{Code, Session};
use hge_ir::{BoxFuture, Mutation, QueryRoot};
use tokio::sync::mpsc;

use super::*;

/// A fake source: a list of integers. Live queries (`sql == "live"`) return the numbers above
/// the cohort's first value; streams (`sql == "stream"`) return up to the second value of
/// them past the cursor (the first value), plus the new cursor.
#[derive(Default)]
struct Numbers {
    rows: Mutex<Vec<i64>>,
    fail: Mutex<bool>,
    executions: AtomicUsize,
}

impl SourceExecutor for Numbers {
    fn execute_query<'a>(
        &'a self,
        _: &'a [&'a QueryRoot],
        _: &'a Session,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        unimplemented!()
    }

    fn execute_mutations<'a>(
        &'a self,
        _: &'a [&'a Mutation],
        _: &'a Session,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        unimplemented!()
    }

    fn execute_multiplexed<'a>(
        &'a self,
        sql: &'a str,
        cohorts: &'a str,
    ) -> BoxFuture<'a, Result<Vec<MultiplexedRow>, Error>> {
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::SeqCst);
            if *self.fail.lock().unwrap() {
                return Err(Error::new(Code::Unexpected, "database query error"));
            }
            let cohorts: Vec<Vec<Option<String>>> = serde_json::from_str(cohorts).unwrap();
            let rows = self.rows.lock().unwrap().clone();
            Ok(cohorts
                .into_iter()
                .enumerate()
                .map(|(index, vars)| {
                    let arg = |i: usize| vars[i].as_deref().unwrap().parse::<i64>().unwrap();
                    let above: Vec<i64> = rows.iter().copied().filter(|r| *r > arg(0)).collect();
                    let columns = if sql == "stream" {
                        let batch: Vec<i64> = above.into_iter().take(arg(1) as usize).collect();
                        let cursor = batch.last().map(|c| c.to_string());
                        vec![Some(serde_json::to_string(&batch).unwrap()), cursor]
                    } else {
                        vec![Some(serde_json::to_string(&above).unwrap())]
                    };
                    MultiplexedRow { index, columns }
                })
                .collect())
        })
    }
}

struct Sink(mpsc::UnboundedSender<String>);

impl Subscriber for Sink {
    fn on_data(&self, fields: &[Option<String>], _cursor: Option<&str>) {
        let _ = self.0.send(fields[0].clone().unwrap_or_default());
    }

    fn on_error(&self, error: &Error) {
        let _ = self.0.send(format!("error: {error}"));
    }
}

fn sink() -> (Arc<dyn Subscriber>, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(Sink(tx)), rx)
}

fn live(min: i64) -> MultiplexedQuery {
    MultiplexedQuery {
        sql: "live".to_owned(),
        variables: vec![Some(min.to_string())],
        cursor: None,
    }
}

fn stream(cursor: i64, batch: i64) -> MultiplexedQuery {
    MultiplexedQuery {
        sql: "stream".to_owned(),
        variables: vec![Some(cursor.to_string()), Some(batch.to_string())],
        cursor: Some(0),
    }
}

fn registry() -> Registry {
    Registry::new(PollerOptions {
        refetch_interval: Duration::from_millis(10),
        batch_size: 2,
    })
}

async fn next(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a message")
        .expect("open channel")
}

/// Let several polls run, then check nothing more arrived.
async fn quiet(rx: &mut mpsc::UnboundedReceiver<String>) {
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(rx.try_recv().ok(), None);
}

#[tokio::test]
async fn cohorts_share_results_and_only_changes_are_sent() {
    let db = Arc::new(Numbers::default());
    *db.rows.lock().unwrap() = vec![1, 2];
    let reg = registry();
    let (a, mut ra) = sink();
    let (b, mut rb) = sink();
    let (c, mut rc) = sink();
    let ha = reg.subscribe(db.clone(), live(0), a).await.unwrap();
    let _hb = reg.subscribe(db.clone(), live(0), b).await.unwrap();
    let _hc = reg.subscribe(db.clone(), live(1), c).await.unwrap();
    assert_eq!(next(&mut ra).await, "[1,2]");
    assert_eq!(next(&mut rb).await, "[1,2]");
    assert_eq!(next(&mut rc).await, "[2]");
    assert_eq!(reg.poller_count(), 1);
    assert_eq!(reg.cohort_count(), 2);
    quiet(&mut ra).await;

    db.rows.lock().unwrap().push(3);
    assert_eq!(next(&mut ra).await, "[1,2,3]");
    assert_eq!(next(&mut rb).await, "[1,2,3]");
    assert_eq!(next(&mut rc).await, "[2,3]");
    quiet(&mut rb).await;

    drop(ha);
    assert_eq!(reg.cohort_count(), 2);
    db.rows.lock().unwrap().push(4);
    assert_eq!(next(&mut rb).await, "[1,2,3,4]");
    assert!(ra.try_recv().is_err());
}

#[tokio::test]
async fn last_unsubscribe_stops_the_poller() {
    let db = Arc::new(Numbers::default());
    let reg = registry();
    let (a, mut ra) = sink();
    let h = reg.subscribe(db.clone(), live(0), a).await.unwrap();
    assert_eq!(next(&mut ra).await, "[]");
    drop(h);
    assert_eq!(reg.poller_count(), 0);
    let executions = db.executions.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(db.executions.load(Ordering::SeqCst), executions);
}

#[tokio::test]
async fn batches_cover_every_cohort() {
    let db = Arc::new(Numbers::default());
    let reg = registry();
    let mut receivers = vec![];
    let mut handles = vec![];
    for i in 0..5 {
        let (s, mut r) = sink();
        handles.push(reg.subscribe(db.clone(), live(i), s).await.unwrap());
        assert_eq!(next(&mut r).await, "[]");
        receivers.push(r);
    }
    assert_eq!(reg.cohort_count(), 5);
    db.rows.lock().unwrap().push(10);
    for r in &mut receivers {
        assert_eq!(next(r).await, "[10]");
    }
}

#[tokio::test]
async fn errors_are_sent_every_poll_then_the_next_result() {
    let db = Arc::new(Numbers::default());
    let reg = registry();
    let (a, mut ra) = sink();
    let _h = reg.subscribe(db.clone(), live(0), a).await.unwrap();
    assert_eq!(next(&mut ra).await, "[]");
    *db.fail.lock().unwrap() = true;
    assert!(next(&mut ra).await.contains("database query error"));
    assert!(next(&mut ra).await.contains("database query error"));
    *db.fail.lock().unwrap() = false;
    // Drain errors sent before the switch.
    let mut msg = next(&mut ra).await;
    while msg.starts_with("error") {
        msg = next(&mut ra).await;
    }
    assert_eq!(msg, "[]");
    quiet(&mut ra).await;
}

#[tokio::test]
async fn start_errors_are_returned() {
    let db = Arc::new(Numbers::default());
    *db.fail.lock().unwrap() = true;
    let reg = registry();
    let (a, _ra) = sink();
    assert!(reg.subscribe(db.clone(), live(0), a).await.is_err());
    assert_eq!(reg.poller_count(), 0);
}

#[tokio::test]
async fn streams_advance_their_cursor() {
    let db = Arc::new(Numbers::default());
    *db.rows.lock().unwrap() = vec![1, 2, 3, 4, 5];
    let reg = registry();
    let (a, mut ra) = sink();
    let _h = reg.subscribe(db.clone(), stream(0, 2), a).await.unwrap();
    assert_eq!(next(&mut ra).await, "[1,2]");
    assert_eq!(next(&mut ra).await, "[3,4]");
    assert_eq!(next(&mut ra).await, "[5]");
    quiet(&mut ra).await;
    db.rows.lock().unwrap().push(6);
    assert_eq!(next(&mut ra).await, "[6]");

    // A subscriber starting from the same cursor joins the advanced cohort.
    let (b, mut rb) = sink();
    let _hb = reg.subscribe(db.clone(), stream(5, 2), b).await.unwrap();
    assert_eq!(next(&mut rb).await, "[6]");
    assert_eq!(reg.cohort_count(), 1);
    db.rows.lock().unwrap().push(7);
    assert_eq!(next(&mut ra).await, "[7]");
    assert_eq!(next(&mut rb).await, "[7]");
    quiet(&mut ra).await;
}

/// [`Numbers`] whose second execution (the first poll) is slow.
struct SlowFirstPoll(Numbers, AtomicUsize);

impl SourceExecutor for SlowFirstPoll {
    fn execute_query<'a>(
        &'a self,
        _: &'a [&'a QueryRoot],
        _: &'a Session,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        unimplemented!()
    }

    fn execute_mutations<'a>(
        &'a self,
        _: &'a [&'a Mutation],
        _: &'a Session,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        unimplemented!()
    }

    fn execute_multiplexed<'a>(
        &'a self,
        sql: &'a str,
        cohorts: &'a str,
    ) -> BoxFuture<'a, Result<Vec<MultiplexedRow>, Error>> {
        Box::pin(async move {
            if self.1.fetch_add(1, Ordering::SeqCst) == 1 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            self.0.execute_multiplexed(sql, cohorts).await
        })
    }
}

/// A stream cohort whose polled subscribers all left mid-poll must not be re-keyed into an
/// empty cohort that keeps its poller running forever.
#[tokio::test]
async fn stream_leaves_no_empty_cohort() {
    let numbers = Numbers::default();
    *numbers.rows.lock().unwrap() = (1..=10).collect();
    let exec: Arc<dyn SourceExecutor> = Arc::new(SlowFirstPoll(numbers, AtomicUsize::new(0)));
    let reg = Registry::new(PollerOptions {
        refetch_interval: Duration::from_millis(100),
        batch_size: 2,
    });
    let (s1, mut rx1) = sink();
    let h1 = reg.subscribe(exec.clone(), stream(0, 2), s1).await.unwrap();
    assert_eq!(next(&mut rx1).await, "[1,2]");
    tokio::time::sleep(Duration::from_millis(150)).await; // the first poll is in flight
    let (s2, mut rx2) = sink();
    let h2 = reg.subscribe(exec.clone(), stream(0, 2), s2).await.unwrap();
    assert_eq!(next(&mut rx2).await, "[1,2]");
    drop(h1);
    tokio::time::sleep(Duration::from_millis(180)).await; // the first poll is done
    drop(h2);
    assert_eq!(reg.cohort_count(), 0);
    assert_eq!(reg.poller_count(), 0);
}
