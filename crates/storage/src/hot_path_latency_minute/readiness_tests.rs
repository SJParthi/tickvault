#[cfg(test)]
mod tests {
    use super::super::*;
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::time::Duration;

    const FIXTURE_DEADLINE: Duration = Duration::from_secs(5);
    const TEST_RETRY: Duration = Duration::from_millis(5);
    const TEST_MINUTE: i64 = 1_793_000_000_000_000_000;

    #[derive(Clone, Debug)]
    struct Request {
        target: String,
        body: Vec<u8>,
    }

    impl Request {
        fn query(&self) -> String {
            reqwest::Url::parse(&format!("http://fixture{}", self.target))
                .expect("request URL")
                .query_pairs()
                .find(|(key, _)| key == "query")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default()
        }

        fn is_write(&self) -> bool {
            self.target.starts_with("/write?")
        }
    }

    struct Server {
        config: QuestDbConfig,
        requests: Arc<Mutex<Vec<Request>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn new(mut answer: impl FnMut(&Request) -> Option<(u16, String)> + Send + 'static) -> Self {
            Self::raw(move |request| {
                answer(request).map(|(status, body)| {
                    format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ).into_bytes()
                })
            })
        }

        fn raw(mut answer: impl FnMut(&Request) -> Option<Vec<u8>> + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let config = QuestDbConfig {
                host: "127.0.0.1".to_owned(),
                http_port: listener.local_addr().expect("address").port(),
                pg_port: 1,
                ilp_port: 1,
            };
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                while !stopping.load(Ordering::Acquire) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(err) => panic!("accept: {err}"),
                    };
                    let request = read_request(&mut stream);
                    captured.lock().expect("requests").push(request.clone());
                    if let Some(response) = answer(&request) {
                        stream
                            .set_write_timeout(Some(FIXTURE_DEADLINE))
                            .expect("write timeout");
                        // A cancellation/oversize fixture deliberately closes its client.
                        let _ = stream.write_all(&response);
                    }
                }
            });
            Self {
                config,
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|r| r.is_write())
                .map(|r| r.body.clone())
                .collect()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                // All fixture reads and callback waits have a finite timeout.
                let _ = thread.join();
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> Request {
        stream
            .set_read_timeout(Some(FIXTURE_DEADLINE))
            .expect("read timeout");
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut chunk).expect("read request");
            assert!(n > 0, "request ended early");
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let headers = std::str::from_utf8(&bytes[..header_end]).expect("headers");
        let target = headers
            .lines()
            .next()
            .expect("request line")
            .split_whitespace()
            .nth(1)
            .expect("target")
            .to_owned();
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>().expect("length"))
            .unwrap_or(0);
        while bytes.len() < header_end + length {
            let n = stream.read(&mut chunk).expect("body");
            assert!(n > 0, "body ended early");
            bytes.extend_from_slice(&chunk[..n]);
        }
        Request {
            target,
            body: bytes[header_end..header_end + length].to_vec(),
        }
    }

    fn columns(keys: &str) -> Value {
        let rows: Vec<_> = keys
            .split(',')
            .map(str::trim)
            .map(|name| {
                let ty = if matches!(name, "ts" | "trading_date_ist") {
                    "TIMESTAMP"
                } else {
                    "SYMBOL"
                };
                json!([name, ty, name == "ts", true])
            })
            .collect();
        json!({"dataset": rows})
    }

    fn healthy_answer(request: &Request) -> Option<(u16, String)> {
        if request.is_write() {
            return Some((204, String::new()));
        }
        let query = request.query();
        let value = if query.contains("FROM tables()") {
            json!({"dataset": [[true, true]]})
        } else if query.contains("FROM table_columns(") {
            let (table, keys) = proof_table_readiness::TABLE_KEYS
                .iter()
                .find(|(table, _)| query.contains(&format!("'{table}'")))
                .expect("known table");
            let mut result = columns(keys);
            if *table == HOT_PATH_LATENCY_MINUTE_TABLE {
                for (name, ty) in HOT_PATH_LATENCY_MINUTE_COLUMNS
                    .iter()
                    .filter(|(name, _)| name.starts_with("strict_"))
                {
                    result["dataset"]
                        .as_array_mut()
                        .expect("columns")
                        .push(json!([name, ty, false, false]));
                }
            }
            result
        } else {
            assert!(query.starts_with("CREATE TABLE") || query.starts_with("ALTER TABLE"));
            json!({"ddl": "OK"})
        };
        Some((200, value.to_string()))
    }

    fn minute_message() -> ProofMessage {
        let mut accumulator = MinuteAccumulator::new();
        let _ = accumulator.absorb(TEST_MINUTE, &SecondWindow::empty());
        ProofMessage::Minute(
            accumulator.rows().expect("minute rows"),
            crate::host_capacity_minute::HostMinuteRow {
                minute_ist_nanos: minute_floor_ist_nanos(TEST_MINUTE),
                process_cpu_pct: Some(12.5),
                ..Default::default()
            },
        )
    }

    fn verdict_message() -> ProofMessage {
        use crate::host_capacity_minute::{CapacityVerdictRow, SignalResult, Verdict};
        ProofMessage::Verdict(CapacityVerdictRow {
            day_ist_nanos: TEST_MINUTE,
            verdict: Verdict::Unknown,
            session_minutes: 1,
            coverage: Default::default(),
            disk_minutes: 0,
            tick_step: (SignalResult::Unknown, None, None),
            ticks_over_1ms: (SignalResult::Pass, 0),
            cpu_wait: (SignalResult::Unknown, None, None),
            disk: (SignalResult::Unknown, 0),
            backlog: (SignalResult::Unknown, None, None),
            worst_host_cpu_busy_pct: Some(12.5),
            process_start_ist_nanos: TEST_MINUTE,
        })
    }

    #[test]
    fn metadata_requires_the_exact_keys_wal_and_designated_timestamp() {
        let flags = json!({"dataset": [[true, true]]});
        for (_, keys) in proof_table_readiness::TABLE_KEYS {
            let good = columns(keys);
            assert!(proof_table_readiness::validate(&flags, &good, keys).is_ok());
            let mut reordered = good.clone();
            reordered["dataset"].as_array_mut().expect("rows").reverse();
            assert!(proof_table_readiness::validate(&flags, &reordered, keys).is_ok());
            for row in 0..good["dataset"].as_array().expect("rows").len() {
                let mut bad = good.clone();
                bad["dataset"][row][3] = json!(false);
                assert!(proof_table_readiness::validate(&flags, &bad, keys).is_err());
            }
            for bad_flags in [
                json!({"dataset": []}),
                json!({"dataset": [[false, true]]}),
                json!({"dataset": [[true, false]]}),
                json!({"dataset": [[true, null]]}),
                json!({"dataset": [[true, true], [true, true]]}),
                json!({"error": "refused"}),
            ] {
                assert!(proof_table_readiness::validate(&bad_flags, &good, keys).is_err());
            }
            let mut extra = good.clone();
            extra["dataset"].as_array_mut().expect("rows").push(json!([
                "histogram",
                "SYMBOL",
                false,
                true
            ]));
            assert!(proof_table_readiness::validate(&flags, &extra, keys).is_err());
            let mut duplicate = good.clone();
            duplicate["dataset"]
                .as_array_mut()
                .expect("rows")
                .push(good["dataset"][0].clone());
            assert!(proof_table_readiness::validate(&flags, &duplicate, keys).is_err());
            for (field, value) in [
                (1, json!("LONG")),
                (2, json!(false)),
                (2, Value::Null),
                (3, Value::Null),
            ] {
                let mut bad = good.clone();
                bad["dataset"][0][field] = value;
                assert!(proof_table_readiness::validate(&flags, &bad, keys).is_err());
            }
            let mut wrong_ts = good.clone();
            wrong_ts["dataset"][0][2] = json!(false);
            wrong_ts["dataset"]
                .as_array_mut()
                .expect("rows")
                .push(json!(["timestamp", "TIMESTAMP", true, false]));
            assert!(proof_table_readiness::validate(&flags, &wrong_ts, keys).is_err());
            assert!(
                proof_table_readiness::validate(&flags, &json!({"dataset": []}), keys).is_err()
            );
        }
    }

    #[tokio::test]
    async fn test_ensure_strict_columns_must_all_be_long_before_any_write() {
        for name in [
            "strict_version",
            "strict_samples",
            "strict_samples_at_or_above_1000ns",
        ] {
            for wrong_type in [None, Some("DOUBLE"), Some("INT"), Some("SYMBOL")] {
                let repaired = Arc::new(AtomicBool::new(false));
                let repaired_on_server = Arc::clone(&repaired);
                let server = Server::new(move |request| {
                    let answer = healthy_answer(request);
                    if !repaired_on_server.load(Ordering::Acquire)
                        && request
                            .query()
                            .contains("table_columns('hot_path_latency_minute')")
                    {
                        let (status, body) = answer.expect("metadata");
                        let mut value: Value = serde_json::from_str(&body).expect("metadata JSON");
                        let rows = value["dataset"].as_array_mut().expect("rows");
                        if let Some(ty) = wrong_type {
                            let row = rows
                                .iter_mut()
                                .find(|row| row[0] == name)
                                .expect("strict column");
                            row[1] = json!(ty);
                        } else {
                            rows.retain(|row| row[0] != name);
                        }
                        return Some((status, value.to_string()));
                    }
                    answer
                });
                let gate = Arc::new(ProofTableReadiness::default());
                let mut writer = HotPathLatencyMinuteWriter::new(&server.config, Arc::clone(&gate));
                writer
                    .append_message(&minute_message())
                    .expect("pending rows");
                assert!(
                    !ensure_hot_path_latency_minute_table(&server.config, false, &gate).await,
                    "{name}: {wrong_type:?}"
                );
                assert!(!gate.keyed.load(Ordering::Acquire));
                assert!(writer.flush().is_err());
                assert!(server.writes().is_empty());
                repaired.store(true, Ordering::Release);
                assert!(ensure_hot_path_latency_minute_table(&server.config, false, &gate).await);
                writer.flush().expect("all types verified");
                assert_eq!(server.writes().len(), 1);
            }
        }
    }

    #[tokio::test]
    async fn test_ensure_hot_path_latency_minute_table_rejects_missing_keys_and_recovers() {
        for (bad_table, _) in proof_table_readiness::TABLE_KEYS {
            let repaired = Arc::new(AtomicBool::new(false));
            let repaired_on_server = Arc::clone(&repaired);
            let server = Server::new(move |request| {
                let answer = healthy_answer(request);
                let query = request.query();
                if !repaired_on_server.load(Ordering::Acquire)
                    && query.contains("FROM table_columns(")
                    && query.contains(&format!("'{bad_table}'"))
                {
                    let (_, keys) = proof_table_readiness::TABLE_KEYS
                        .iter()
                        .find(|(t, _)| *t == bad_table)
                        .expect("table");
                    let mut value = columns(keys);
                    value["dataset"][1][3] = json!(false);
                    return Some((200, value.to_string()));
                }
                answer
            });
            let gate = Arc::new(ProofTableReadiness::default());
            let mut writer = HotPathLatencyMinuteWriter::new(&server.config, Arc::clone(&gate));
            writer.append_message(&minute_message()).expect("minute");
            writer.append_message(&verdict_message()).expect("verdict");
            let expected = writer.buffer.as_bytes().to_vec();
            assert!(!ensure_hot_path_latency_minute_table(&server.config, false, &gate).await);
            assert!(!gate.keyed.load(Ordering::Acquire));
            assert!(writer.flush().is_err());
            assert_eq!(writer.buffer.as_bytes(), expected);
            assert!(
                server.writes().is_empty(),
                "HTTP-successful DDL is not keyed readiness"
            );
            repaired.store(true, Ordering::Release);
            assert!(ensure_hot_path_latency_minute_table(&server.config, false, &gate).await);
            writer.flush().expect("verified keys permit delivery");
            assert_eq!(server.writes(), vec![expected]);
            assert_eq!(writer.pending(), 0);
        }
    }

    #[tokio::test]
    async fn query_failures_never_publish_readiness() {
        for response in [
            Some((500, json!({"error": "refused"}).to_string())),
            Some((200, json!({"error": "SQL refusal"}).to_string())),
            Some((200, "not JSON".to_owned())),
            None,
        ] {
            let server = Server::new(move |_| response.clone());
            let gate = ProofTableReadiness::default();
            assert!(!ensure_hot_path_latency_minute_table(&server.config, false, &gate).await);
            assert!(!gate.keyed.load(Ordering::Acquire));
            assert!(server.writes().is_empty());
        }
    }

    #[tokio::test]
    async fn a_cancelled_ensure_cannot_open_the_gate_or_send_early_rows() {
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let server = Server::new(move |request| {
            started_tx.try_send(()).expect("started");
            let _ = release_rx.recv_timeout(FIXTURE_DEADLINE);
            healthy_answer(request)
        });
        let gate = Arc::new(ProofTableReadiness::default());
        let mut writer = HotPathLatencyMinuteWriter::new(&server.config, Arc::clone(&gate));
        writer
            .append_message(&minute_message())
            .expect("queued minute");
        let expected = writer.buffer.as_bytes().to_vec();
        let config = server.config.clone();
        let ensuring = Arc::clone(&gate);
        let task = tokio::spawn(async move {
            ensure_hot_path_latency_minute_table(&config, false, &ensuring).await
        });
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(FIXTURE_DEADLINE))
            .await
            .expect("barrier task")
            .expect("DDL started");
        assert!(writer.flush().is_err());
        assert!(server.writes().is_empty());
        task.abort();
        assert!(task.await.expect_err("cancelled").is_cancelled());
        release_tx.send(()).expect("release server");
        assert!(!gate.keyed.load(Ordering::Acquire));
        assert_eq!(
            writer.buffer.as_bytes(),
            expected,
            "cancellation did not persist these rows"
        );
    }

    #[tokio::test]
    async fn retained_rows_flush_after_readiness_without_another_producer_message() {
        let (posted_tx, posted_rx) = crossbeam_channel::bounded(1);
        let server = Server::new(move |request| {
            if request.is_write() {
                posted_tx.try_send(request.body.clone()).expect("posted");
            }
            healthy_answer(request)
        });
        let gate = Arc::new(ProofTableReadiness::default());
        let other_gate = ProofTableReadiness::default();
        let mut writer = HotPathLatencyMinuteWriter::new(&server.config, Arc::clone(&gate));
        writer.append_message(&minute_message()).expect("minute");
        let expected = writer.buffer.as_bytes().to_vec();
        assert!(ensure_hot_path_latency_minute_table(&server.config, false, &other_gate).await);
        assert!(
            writer.flush().is_err(),
            "another startup's gate cannot open this writer"
        );
        assert!(server.writes().is_empty());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || run_writer(rx, writer, TEST_RETRY));
        assert!(ensure_hot_path_latency_minute_table(&server.config, false, &gate).await);
        let posted = tokio::task::spawn_blocking(move || posted_rx.recv_timeout(FIXTURE_DEADLINE))
            .await
            .expect("receiver task")
            .expect("timer delivered retained rows");
        assert_eq!(posted, expected);
        drop(tx); // No producer message was sent through this channel.
        let writer = worker
            .join()
            .expect("worker exits when the local sender closes");
        assert_eq!(writer.pending(), 0);
        assert!(writer.buffer.as_bytes().is_empty());
    }

    #[test]
    fn an_unready_writer_drains_a_finite_queue_and_returns_unpersisted_residue_on_disconnect() {
        let (tx, rx) = crossbeam_channel::bounded(2);
        tx.try_send(minute_message()).expect("first slot");
        tx.try_send(verdict_message()).expect("second slot");
        assert!(matches!(
            tx.try_send(minute_message()),
            Err(crossbeam_channel::TrySendError::Full(_))
        ));
        let worker = std::thread::spawn(move || {
            run_writer(rx, HotPathLatencyMinuteWriter::for_test(), TEST_RETRY)
        });
        drop(tx);
        let residue = worker.join().expect("disconnected worker");
        assert_eq!(residue.pending(), STAGES_PER_MINUTE + 2);
        assert!(
            !residue.buffer.as_bytes().is_empty(),
            "disconnect is not successful persistence"
        );
    }

    #[test]
    fn unready_flushes_use_the_shared_pending_row_cap() {
        let ProofMessage::Minute(rows, _) = minute_message() else {
            panic!("minute");
        };
        let mut writer = HotPathLatencyMinuteWriter::for_test();
        let cap = crate::ilp_overflow::MAX_PENDING_ROWS;
        for _ in 0..cap - 1 {
            writer.append_row(&rows[0]).expect("row");
        }
        let bytes = writer.buffer.as_bytes().to_vec();
        for _ in 0..2 {
            assert!(writer.flush().is_err());
            assert_eq!(writer.pending(), cap - 1);
            assert_eq!(writer.buffer.as_bytes(), bytes);
        }
        writer.append_row(&rows[0]).expect("cap row");
        let error = writer.flush().expect_err("unready");
        assert_eq!(
            writer.pending(),
            0,
            "the readiness gate cannot bypass the cap"
        );
        assert!(writer.buffer.as_bytes().is_empty());
        assert!(
            error
                .to_string()
                .contains(&format!("{cap} row(s) were discarded"))
        );
    }

    #[test]
    fn a_failed_thread_spawn_does_not_poison_the_sink_and_second_start_does_not_spawn() {
        let mut sink = OnceLock::new();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        assert!(
            spawn_writer_into(
                &sink,
                &config,
                Arc::new(ProofTableReadiness::default()),
                |_, _| { Err(std::io::Error::other("injected spawn failure")) }
            )
            .is_err()
        );
        assert!(sink.get().is_none());
        let worker = spawn_writer_into(
            &sink,
            &config,
            Arc::new(ProofTableReadiness::default()),
            |rx, writer| {
                std::thread::Builder::new().spawn(move || {
                    let _residue = run_writer(rx, writer, TEST_RETRY);
                })
            },
        )
        .expect("later spawn can install");
        assert!(
            spawn_writer_into(
                &sink,
                &config,
                Arc::new(ProofTableReadiness::default()),
                |_, _| { panic!("second start must not spawn another thread") }
            )
            .is_err()
        );
        drop(sink.take());
        worker.join().expect("local writer stopped");
    }

    #[test]
    fn a_concurrent_install_loser_disconnects_its_unpublished_worker() {
        let sink = OnceLock::new();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let (winner_tx, _winner_rx) = crossbeam_channel::bounded(1);
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let result = spawn_writer_into(
            &sink,
            &config,
            Arc::new(ProofTableReadiness::default()),
            |rx, writer| {
                sink.set(winner_tx).expect("concurrent winner");
                std::thread::Builder::new().spawn(move || {
                    let residue = run_writer(rx, writer, TEST_RETRY);
                    done_tx.send(residue.pending()).expect("completion");
                })
            },
        );
        assert!(result.is_err());
        assert_eq!(
            done_rx
                .recv_timeout(FIXTURE_DEADLINE)
                .expect("loser exited"),
            0
        );
    }

    #[tokio::test]
    async fn an_ensure_request_timeout_never_reaches_metadata_verification() {
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let server = Server::new(move |request| {
            started_tx.try_send(()).expect("started");
            let _ = release_rx.recv_timeout(FIXTURE_DEADLINE);
            healthy_answer(request)
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .expect("client");
        let url = format!(
            "http://{}:{}/exec",
            server.config.host, server.config.http_port
        );
        let task = tokio::spawn(async move { ensure_proof_tables_once(&client, &url).await });
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(FIXTURE_DEADLINE))
            .await
            .expect("barrier task")
            .expect("request reached server");
        assert!(task.await.expect("ensure task").is_err());
        release_tx.send(()).expect("release timed-out request");
        let requests = server.requests.lock().expect("requests");
        assert!(
            requests
                .iter()
                .all(|request| !request.query().starts_with("SELECT"))
        );
        assert!(
            !requests.is_empty(),
            "the timeout must exercise a real request"
        );
    }

    #[tokio::test]
    async fn oversized_declared_response_fails_before_body_read_and_never_opens_readiness() {
        let declared = proof_table_readiness::QUERY_RESPONSE_MAX_BYTES + 1;
        let server = Server::raw(move |_| {
            Some(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
        ).into_bytes())
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(FIXTURE_DEADLINE)
            .build()
            .expect("client");
        let url = format!(
            "http://{}:{}/exec",
            server.config.host, server.config.http_port
        );
        let error = ensure_proof_tables_once(&client, &url)
            .await
            .expect_err("oversized length");
        assert!(
            format!("{error:#}").contains("declared response exceeds"),
            "reject headers before attempting to read the deliberately absent body: {error:#}"
        );
        let gate = ProofTableReadiness::default();
        assert!(!ensure_hot_path_latency_minute_table(&server.config, false, &gate).await);
        assert!(!gate.keyed.load(Ordering::Acquire));
        assert!(server.writes().is_empty());
    }

    #[derive(Clone, Copy, Debug)]
    enum BodyFraming {
        CloseDelimited,
        Chunked,
        ChunkedWithFalseLength,
    }

    fn framed_response(body: &str, framing: BodyFraming) -> Vec<u8> {
        let extra_headers = match framing {
            BodyFraming::CloseDelimited => "",
            BodyFraming::Chunked => "Transfer-Encoding: chunked\r\n",
            BodyFraming::ChunkedWithFalseLength => {
                "Content-Length: 1\r\nTransfer-Encoding: chunked\r\n"
            }
        };
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra_headers}Connection: close\r\n\r\n"
        ).into_bytes();
        if matches!(framing, BodyFraming::CloseDelimited) {
            response.extend_from_slice(body.as_bytes());
        } else {
            // Several sub-limit chunks must still hit the cumulative byte cap.
            const HTTP_CHUNK_BYTES: usize = 4096;
            for chunk in body.as_bytes().chunks(HTTP_CHUNK_BYTES) {
                response.extend_from_slice(format!("{:X}\r\n", chunk.len()).as_bytes());
                response.extend_from_slice(chunk);
                response.extend_from_slice(b"\r\n");
            }
            response.extend_from_slice(b"0\r\n\r\n");
        }
        response
    }

    #[tokio::test]
    async fn streamed_ddl_and_metadata_responses_obey_the_cap_without_trusting_content_length() {
        let cap = proof_table_readiness::QUERY_RESPONSE_MAX_BYTES;
        for framing in [
            BodyFraming::CloseDelimited,
            BodyFraming::Chunked,
            BodyFraming::ChunkedWithFalseLength,
        ] {
            for metadata in [false, true] {
                for size in [cap, cap + 1] {
                    let server = Server::raw(move |request| {
                        let (status, mut body) = healthy_answer(request).expect("healthy response");
                        assert_eq!(status, 200, "ensure never issues ILP");
                        let target = if metadata {
                            request.query().contains("FROM tables()")
                        } else {
                            request.query().starts_with("CREATE TABLE")
                        };
                        let response = if target {
                            // Valid JSON plus whitespace. Without the bound this
                            // would pass all DDL/key checks, making the test nonvacuous.
                            assert!(body.len() < size);
                            body.extend(std::iter::repeat_n(' ', size - body.len()));
                            framed_response(&body, framing)
                        } else {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                            ).into_bytes()
                        };
                        Some(response)
                    });
                    let gate = ProofTableReadiness::default();
                    let ready =
                        ensure_hot_path_latency_minute_table(&server.config, false, &gate).await;
                    assert_eq!(
                        ready,
                        size == cap,
                        "{framing:?}, metadata={metadata}, bytes={size}"
                    );
                    assert_eq!(gate.keyed.load(Ordering::Acquire), size == cap);
                    assert!(server.writes().is_empty());
                }
            }
        }
    }
}
