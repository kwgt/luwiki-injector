/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 観測無効時の型互換と、同期点による停止・所有・実HTTPの制御検証
//!

#[cfg(not(feature = "measurement"))]
///
/// 無効featureの所有型が通常型であることを確認する。
///
/// # 概要
/// 一時領域とfileを通常型として扱い、permitの所有数と解放を検査する。
///
/// # 注記
/// 同じArcを移譲し、強参照を増やさず返すことも確認する。
///
/// # 戻り値
/// なし。型または所有数が異なる場合は失敗する。
///
#[tokio::test]
async fn disabled_observation_preserves_ordinary_ownership() {
    /*
     * 観測無効時の一時領域とfileを通常型で作成・解放する
     */
    let directory: tempfile::TempDir = super::area(super::Role::Other)
        .expect("control directory");
    let file: std::fs::File = super::file(std::fs::File::create(
        directory.path().join("control")).unwrap(), super::Role::Other);
    drop(file);
    /*
     * permitの移譲で所有数が増えず、破棄で利用可能数が戻ることを確認する
     */
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let owned = semaphore.clone().acquire_owned().await.unwrap();
    let original = std::sync::Arc::new(owned);
    let count = std::sync::Arc::strong_count(&original);
    let returned: std::sync::Arc<tokio::sync::OwnedSemaphorePermit> =
        super::permit(original);
    assert_eq!(std::sync::Arc::strong_count(&returned), count);
    drop(returned);
    assert_eq!(semaphore.available_permits(), 1);
}

#[cfg(feature = "measurement")]
mod enabled {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use tokio::sync::oneshot;

    use crate::source::{SourceError, StopContext};

    use crate::measurement::{self, Event, Role};

    ///
    /// workerが領域を保持する区間を同期点で固定する。
    ///
    /// # 引数
    /// * `parent` - この制御試験の一時領域
    /// * `stop` - 試験する共有context
    /// * `ready` - workerが所有を開始した通知
    /// * `release` - 所有区間を終える通知
    ///
    /// # 戻り値
    /// 通常のworker結果。停止時は領域をworkerで破棄する。
    ///
    async fn held_worker(parent: PathBuf, stop: StopContext,
        ready: oneshot::Sender<PathBuf>, release: mpsc::Receiver<()>)
        -> Result<measurement::Area, SourceError>
    {
        crate::source::run_blocking(stop, move |stop| {
            let area = measurement::area_in(&parent, Role::Source)
                .map_err(|_| SourceError::Io)?;
            let _ = ready.send(area.path().to_owned());
            release.recv().map_err(|_| SourceError::Io)?;
            stop.checkpoint()?;
            Ok(area)
        }).await
    }

    ///
    /// 実transportを二回だけ受けるloopbackを用意する。
    ///
    /// # 概要
    /// 有限のaccept待機で二GETだけに合成応答を返す。
    /// 認証headerは確認用buffer以外へ保持しない。
    ///
    /// # 戻り値
    /// URLと終了待機handle。認証原文は保存/表示しない。
    ///
    fn server() -> (url::Url, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let target = url::Url::parse(&format!("http://{}",
            listener.local_addr().unwrap())).unwrap();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            for body in [
                b"{\"id\":\"00000000000000000000000000\"}".as_slice(),
                b"measurement-body-canary\n".as_slice(),
            ] {
                /*
                 * acceptの待機は有限にし、失敗時にもhandlerを残さない
                 */
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind()
                            == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline,
                                "control accept deadline");
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => panic!("control accept"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0_u8; 1];
                    assert_eq!(stream.read(&mut byte).unwrap(), 1);
                    header.push(byte[0]);
                    assert!(header.len() < 16384, "control header bound");
                }
                assert!(header.starts_with(b"GET "), "control method");
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                    Connection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        (target, worker)
    }

    ///
    /// writerが受け付けたworker終了eventを有限時間待つ。
    ///
    /// # 概要
    /// 終了eventを反復集計し、必要数への到達または待機期限を検査する。
    ///
    /// # 引数
    /// * `directory` - 制御試験の保護出力
    /// * `minimum` - 終了済みworkerの必要数
    ///
    /// # 戻り値
    /// なし。future dropの通知をworker終了へ読み替えない。
    ///
    async fn workers_recorded(directory: &Path, minimum: usize) {
        let until = Instant::now() + Duration::from_secs(5);
        let mut read_errors = 0;
        loop {
            /*
             * 読取失敗を数えつつ、記録済みのworker終了eventを集計する
             */
            let text = fs::read_to_string(directory.join("events.tsv"))
                .unwrap_or_else(|_| { read_errors += 1; String::new() });
            let count = text.lines().filter(|line|
                line.split('\t').nth(1) == Some("7")).count();
            /*
             * 必要数に達したら完了し、未到達なら期限内だけ待機する
             */
            if count >= minimum { return; }
            assert!(Instant::now() < until,
                "control worker completion: expected={minimum} read={count} \
                bytes={} read_errors={read_errors} \
                state=(emitted,written,valid,live,writer_finished)={:?}",
                text.len(), measurement::active::control_state());
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    ///
    /// 実checkpoint/worker/transportを通して観測契約を確認する。
    ///
    /// # 概要
    /// C/D/Fは別contextと同期点で注入し、所有が続く区間と終了を分ける。
    /// 最後に実loopback GETと受信を行い、完全な有限traceを検査する。
    ///
    /// # 注記
    /// 単独filter・test-threads=1で実行する。外部Wiki/envを読まない。
    ///
    /// # 戻り値
    /// なし。期限/所有/非露出/実要求数に不一致があれば失敗する。
    ///
    #[tokio::test]
    async fn controlled_observation() {
        /*
         * shellのumaskに依存せず保護条件を満たす領域を最初から作る
         */
        let output = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700)).tempdir().unwrap();
        let temporary = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700)).tempdir().unwrap();
        let output_path = output.path().canonicalize().unwrap();
        let temporary_path = temporary.path().canonicalize().unwrap();
        assert_eq!(fs::metadata(&output_path).unwrap()
            .permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&temporary_path).unwrap()
            .permissions().mode() & 0o777, 0o700);
        assert!(measurement::active::overflow_control());
        measurement::active::init_at(&output_path, &temporary_path);
        assert!(measurement::active::control_state().2,
            "control recorder initialization");

        /*
         * 共有取消Cでもworkerが所有を保持してから解放する
         */
        let stop = StopContext::new(Instant::now() + Duration::from_secs(30));
        let context = stop.clone();
        let (ready, started) = oneshot::channel();
        let (release, held) = mpsc::channel();
        let parent = temporary_path.clone();
        let task = tokio::spawn(held_worker(parent, context, ready, held));
        let path = started.await.unwrap();
        stop.cancel();
        assert!(path.exists() && !task.is_finished());
        release.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(SourceError::Cancelled)));
        assert!(!path.exists());

        /*
         * 同じ絶対期限Dの後もjoin中の所有区間を観測する
         */
        let deadline = Instant::now() + Duration::from_secs(2);
        let stop = StopContext::new(deadline);
        let (ready, started) = oneshot::channel();
        let (release, held) = mpsc::channel();
        let parent = temporary_path.clone();
        let task = tokio::spawn(held_worker(parent, stop, ready, held));
        let path = started.await.unwrap();
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline
            + Duration::from_millis(10))).await;
        assert!(path.exists() && !task.is_finished());
        release.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(SourceError::Deadline)));
        assert!(!path.exists());

        /*
         * 実future drop Fはworker終了と別であることを確認する
         */
        let stop = StopContext::new(Instant::now() + Duration::from_secs(30));
        let context = stop.clone();
        let (ready, started) = oneshot::channel();
        let (release, held) = mpsc::channel();
        let parent = temporary_path.clone();
        let task = tokio::spawn(held_worker(parent, context, ready, held));
        let path = started.await.unwrap();
        measurement::event(Event::FutureDrop, stop.trace_id(), 0,
            Role::Other, 0);
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert!(matches!(stop.checkpoint(), Err(SourceError::Cancelled)));
        assert!(path.exists());
        release.send(()).unwrap();
        workers_recorded(&output_path, 3).await;
        assert!(!path.exists());

        /*
         * S親子・P/J・file close・最後のpermit解放を区別する
         */
        let source = measurement::area_in(&temporary_path, Role::Source)
            .unwrap();
        let nested = measurement::area_in(source.path(), Role::Source)
            .unwrap();
        let payload = measurement::area_in(&temporary_path, Role::Payload)
            .unwrap();
        let json = measurement::area_in(&temporary_path, Role::Json).unwrap();
        let empty = measurement::area_in(&temporary_path, Role::Wiki).unwrap();
        fs::write(source.path().join("original"), b"abc").unwrap();
        fs::hard_link(source.path().join("original"),
            nested.path().join("alias")).unwrap();
        fs::write(json.path().join("empty"), b"").unwrap();
        let mut file = measurement::file(fs::File::create(
            payload.path().join("control")).unwrap(), Role::Payload);
        file.write_all(b"measurement-file-canary").unwrap();
        drop(file);
        /*
         * 同時容量と空fileを確認し、親子/hardlinkを二重加算しない
         */
        let paths = vec![(1, source.path().to_owned(), Role::Source),
            (2, nested.path().to_owned(), Role::Source),
            (3, payload.path().to_owned(), Role::Payload),
            (4, json.path().to_owned(), Role::Json),
            (5, empty.path().to_owned(), Role::Wiki)];
        let (sizes, errors) = measurement::active::storage_scan(&paths);
        assert_eq!(errors, 0);
        assert_eq!(sizes[1][0], 3);
        assert_eq!(sizes[1][2..], [1, 2]);
        assert_eq!(sizes[2][0], 23);
        assert_eq!(sizes[2][2..], [1, 1]);
        assert_eq!(sizes[4][0], 0);
        assert_eq!(sizes[4][2..], [1, 1]);
        assert_eq!(sizes[0], [0; 4]);
        assert_eq!(sizes[3], [0, 0, 0, 1]);
        assert_eq!(sizes[1][1],
            std::os::unix::fs::MetadataExt::blocks(
                &fs::metadata(source.path().join("original")).unwrap()) * 512);
        fs::hard_link(source.path().join("original"),
            payload.path().join("shared")).unwrap();
        let (shared_sizes, shared_errors) =
            measurement::active::storage_scan(&paths);
        assert_eq!(shared_errors, 0);
        assert_eq!(shared_sizes[0][0], 3);
        assert_eq!(shared_sizes[0][2], 1);
        assert_eq!(shared_sizes[1][0], 0);
        assert_eq!(shared_sizes[2][0], 23);
        fs::remove_file(payload.path().join("shared")).unwrap();
        let missing = vec![(5, json.path().join("missing"), Role::Json)];
        assert_eq!(measurement::active::storage_scan(&missing).1, 1);
        std::os::unix::fs::symlink(source.path(),
            json.path().join("unsupported-link")).unwrap();
        assert!(measurement::active::storage_scan(&paths).1 > 0);
        fs::remove_file(json.path().join("unsupported-link")).unwrap();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = measurement::permit(Arc::new(semaphore.clone()
            .acquire_owned().await.unwrap()));
        let shared = measurement::permit(measurement::share(&permit));
        drop(permit);
        assert_eq!(semaphore.available_permits(), 0);
        drop(shared);
        assert_eq!(semaphore.available_permits(), 1);
        drop((nested, source, payload, json, empty));

        /*
         * 実transportの初回poll・受信・取消後HTTP0を確認する
         */
        let (target, server) = server();
        let deadline = Instant::now() + Duration::from_secs(20);
        let config = crate::cmd_args::resolve::wiki_config_fixture(
            &target, true, deadline);
        let stop = StopContext::new(deadline);
        let client = crate::wiki_client::WikiClient::new(&config,
            stop.clone()).unwrap();
        let path = crate::report::types::PagePath::new(
            "/measurement-control").unwrap();
        let _ = client.lookup(&path).await.unwrap();
        let id = crate::report::types::PageId::new(
            "00000000000000000000000000").unwrap();
        let body = client.read_source(&id).await.unwrap();
        assert_eq!(body.byte_len(), 24);
        drop(body);
        stop.cancel();
        assert!(client.lookup(&path).await.is_err());
        server.join().unwrap();
        drop(client);
        workers_recorded(&output_path, 5).await;
        measurement::finish();

        /*
         * 生の資格/本文/URLを含まない完全traceだけを受け入れる
         */
        assert_eq!(fs::read(output_path.join("complete")).unwrap(),
            b"complete\n");
        let trace = fs::read_to_string(output_path.join("events.tsv"))
            .unwrap();
        assert!(trace.ends_with("END\t1\n"));
        assert!(trace.starts_with("LUWIKI_TRACE\t2\n"));
        /*
         * 実Dropが同snapshotへS/Pと空JSONの存在を記録したことを確認する
         */
        let rows: Vec<Vec<u64>> = trace.lines().filter_map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 6 { return None; }
            Some(fields.iter().map(|s| s.parse().unwrap()).collect())
        }).collect();
        let captured = rows.iter().any(|row| {
            row[1] == Event::StorageStart as u64 && [
                (Event::StorageLogical, 1, 3),
                (Event::StorageFiles, 1, 1),
                (Event::StorageAreas, 1, 2),
                (Event::StorageLogical, 2, 23),
                (Event::StorageLogical, 4, 0),
                (Event::StorageFiles, 4, 1),
                (Event::StorageAreas, 4, 1),
                (Event::StorageEnd, 0, 0),
            ].iter().all(|(kind, role, value)| rows.iter().any(|other|
                other[2] == row[2] && other[1] == *kind as u64
                    && other[4] == *role && other[5] == *value))
        });
        assert!(captured, "simultaneous source/payload and empty JSON");
        for canary in ["dummy-wiki-secret", "measurement-body-canary",
            "measurement-file-canary", "http://", "Authorization"]
        { assert!(!trace.contains(canary), "trace non-disclosure"); }
        let events: Vec<_> = trace.lines().filter_map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 6 { return None; }
            Some((fields[1].parse::<u8>().unwrap(),
                fields[2].parse::<u64>().unwrap()))
        }).collect();
        assert_eq!(events.iter().filter(|(e, _)|
            *e == Event::HttpStart as u8).count(), 2);
        for kind in [Event::Cancel, Event::DeadlineSeen, Event::FutureDrop,
            Event::FileDropEnd, Event::PermitRelease, Event::AreaDropEnd]
        { assert!(events.iter().any(|(e, _)| *e == kind as u8)); }
        for (_, id) in events.iter().filter(|(e, _)|
            *e == Event::WorkerScheduled as u8)
        {
            assert!(events.contains(&(Event::WorkerEnd as u8, *id)));
        }
    }
}
