use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::*;

#[derive(Default)]
struct ScriptedFetcher {
    responses: Mutex<BTreeMap<String, ProbeResponse>>,
    requests: Mutex<Vec<FetchRequest>>,
    delay: Duration,
    active: AtomicUsize,
    peak: AtomicUsize,
}

fn url(s: &str) -> Url {
    Url::parse(s).unwrap_or_else(|e| panic!("test URL: {e}"))
}

fn response(path: &str, status: u16, ct: &str, body: &[u8]) -> ProbeResponse {
    ProbeResponse {
        url: url(&format!("https://cdn.example{path}")),
        status,
        headers: BTreeMap::from([("content-type".into(), ct.into())]),
        body: body.to_vec(),
        truncated: false,
    }
}

fn stream(path: &str, format: Format) -> Stream {
    Stream::new(url(&format!("https://cdn.example{path}")), format)
}

impl ScriptedFetcher {
    fn reply(&self, path: &str, response: ProbeResponse) {
        self.responses
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .insert(path.into(), response);
    }
    fn requests(&self) -> Vec<FetchRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .clone()
    }
}

#[async_trait]
impl Fetcher for ScriptedFetcher {
    async fn request(&self, _: FetchRequest) -> Result<FetchResponse, FetchError> {
        panic!("media probes must use the bounded binary API")
    }
    async fn probe(
        &self,
        request: FetchRequest,
        limit: usize,
    ) -> Result<Option<ProbeResponse>, FetchError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let path = request.url.path().to_string();
        self.requests
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .push(request);
        let mut response = self
            .responses
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .get(&path)
            .cloned();
        if let Some(response) = response.as_mut() {
            response.truncated |= response.body.len() >= limit;
            response.body.truncate(limit);
        }
        Ok(response)
    }
}

fn probe() -> StreamProbe {
    StreamProbe::new(ProbeConfig::default())
}

#[tokio::test]
async fn follows_redirected_master_child_and_segment_with_playback_headers() {
    let fetcher = ScriptedFetcher::default();
    let mut master = response(
        "/entry",
        200,
        "text/html",
        b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=99\n720/index.m3u8\n",
    );
    master.url = url("https://cdn.example/redirect/master.m3u8");
    fetcher.reply("/entry", master);
    fetcher.reply(
        "/redirect/720/index.m3u8",
        response(
            "/redirect/720/index.m3u8",
            200,
            "application/x-mpegurl",
            b"#EXTM3U\n#EXTINF:6,\nfirst.ts\n",
        ),
    );
    let mut ts = vec![0; 189];
    ts[0] = 0x47;
    ts[188] = 0x47;
    fetcher.reply(
        "/redirect/720/first.ts",
        response("/redirect/720/first.ts", 206, "image/jpeg", &ts),
    );
    let mut card = stream("/entry", Format::Hls);
    card.meta
        .request_headers
        .insert("Referer".into(), "https://origin.example/".into());
    card.meta
        .request_headers
        .insert("Origin".into(), "https://origin.example".into());
    card.meta
        .request_headers
        .insert("Range".into(), "bytes=999-".into());
    assert_eq!(probe().check(&fetcher, &card).await, Verdict::Alive);
    let requests = fetcher.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].headers.contains_key("range"));
    assert_eq!(
        requests[2].headers.get("range").map(String::as_str),
        Some("bytes=0-2047")
    );
    for request in requests {
        assert_eq!(
            request.headers.get("referer").map(String::as_str),
            Some("https://origin.example/")
        );
        assert_eq!(
            request.headers.get("origin").map(String::as_str),
            Some("https://origin.example")
        );
    }
}

#[tokio::test]
async fn dead_segments_drop_live_masters() {
    for (status, ct, body) in [
        (404, "text/plain", "gone"),
        (200, "video/mp2t", "<html>error</html>"),
    ] {
        let fetcher = ScriptedFetcher::default();
        fetcher.reply(
            "/master.m3u8",
            response(
                "/master.m3u8",
                200,
                "text/plain",
                b"#EXTM3U\n#EXTINF:1,\npart.ts\n",
            ),
        );
        fetcher.reply(
            "/part.ts",
            response("/part.ts", status, ct, body.as_bytes()),
        );
        assert_eq!(
            probe()
                .check(&fetcher, &stream("/master.m3u8", Format::Hls))
                .await,
            Verdict::Dead
        );
    }
}

#[tokio::test]
async fn direct_files_reject_archives_html_json_and_thumbnails() {
    let cases = [
        (
            "application/octet-stream",
            b"PK\x03\x04\0archive".as_slice(),
        ),
        ("application/zip", b"P"),
        ("video/mp4", b"<html>not a movie</html>"),
        ("text/html", b"a login page"),
        ("application/json", b"{\"error\":\"not found\"}"),
        ("video/mp4", b"\x89PNG\r\n\x1a\n\0"),
    ];
    for (ct, bytes) in cases {
        let fetcher = ScriptedFetcher::default();
        fetcher.reply("/file", response("/file", 200, ct, bytes));
        assert_eq!(
            probe().check(&fetcher, &stream("/file", Format::Mp4)).await,
            Verdict::Dead,
            "{ct}"
        );
    }
}

#[tokio::test]
async fn filename_archive_and_real_media_with_lying_mime_types() {
    let fetcher = ScriptedFetcher::default();
    let mut zip = response("/zip", 206, "application/octet-stream", b"P");
    zip.headers.insert(
        "content-disposition".into(),
        "attachment; filename*=UTF-8''Some%20Show%2Ezip".into(),
    );
    fetcher.reply("/zip", zip);
    fetcher.reply(
        "/movie",
        response("/movie", 206, "text/html", b"\0\0\0\x18ftypisom"),
    );
    assert_eq!(
        probe().check(&fetcher, &stream("/zip", Format::Mp4)).await,
        Verdict::Dead
    );
    assert_eq!(
        probe()
            .check(&fetcher, &stream("/movie", Format::Mp4))
            .await,
        Verdict::Alive
    );
}

#[tokio::test]
async fn blocking_and_transient_statuses_are_never_dead() {
    let fetcher = ScriptedFetcher::default();
    for status in [401, 403, 429, 500, 502, 503] {
        fetcher.reply(
            "/file",
            response("/file", status, "text/html", b"<html>blocked</html>"),
        );
        assert_eq!(
            probe().check(&fetcher, &stream("/file", Format::Mp4)).await,
            Verdict::Unknown
        );
    }
    let mut cf = response("/file", 200, "text/html", b"<html>challenge</html>");
    cf.headers.insert("cf-mitigated".into(), "challenge".into());
    fetcher.reply("/file", cf);
    assert_eq!(
        probe().check(&fetcher, &stream("/file", Format::Mp4)).await,
        Verdict::Unknown
    );
}

#[tokio::test]
async fn coalesces_identical_probes_but_separates_playback_headers() {
    let fetcher = ScriptedFetcher {
        delay: Duration::from_millis(10),
        ..Default::default()
    };
    fetcher.reply("/file", response("/file", 404, "text/html", b"missing"));
    let gate = probe();
    let card = stream("/file", Format::Mp4);
    let (a, b) = tokio::join!(gate.check(&fetcher, &card), gate.check(&fetcher, &card));
    assert_eq!((a, b), (Verdict::Dead, Verdict::Dead));
    assert_eq!(fetcher.requests().len(), 1);
    let mut other = card.clone();
    other
        .meta
        .request_headers
        .insert("Referer".into(), "https://different.example/".into());
    gate.check(&fetcher, &other).await;
    assert_eq!(fetcher.requests().len(), 2);
    other.meta.request_headers =
        BTreeMap::from([("referer".into(), "https://different.example/".into())]);
    gate.check(&fetcher, &other).await;
    assert_eq!(fetcher.requests().len(), 2);
}

#[tokio::test]
async fn dead_verdicts_expire_and_do_not_poison_other_files_on_host() {
    let fetcher = ScriptedFetcher::default();
    fetcher.reply("/file", response("/file", 404, "text/plain", b"gone"));
    fetcher.reply(
        "/other",
        response("/other", 206, "video/mp4", b"\0\0\0\x18ftypisom"),
    );
    let gate = StreamProbe::new(ProbeConfig {
        dead_ttl: Duration::from_millis(10),
        ..Default::default()
    });
    let card = stream("/file", Format::Mp4);
    assert_eq!(gate.check(&fetcher, &card).await, Verdict::Dead);
    assert_eq!(
        gate.check(&fetcher, &stream("/other", Format::Mp4)).await,
        Verdict::Alive
    );
    fetcher.reply(
        "/file",
        response("/file", 206, "video/mp4", b"\0\0\0\x18ftypisom"),
    );
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(gate.check(&fetcher, &card).await, Verdict::Alive);
}

#[tokio::test]
async fn external_and_single_use_links_are_not_consumed() {
    let fetcher = ScriptedFetcher::default();
    let mut external = stream("/embed", Format::Unknown);
    external.is_external = true;
    let single_use = Stream::new(
        url("https://video-downloads.googleusercontent.com/token"),
        Format::Mp4,
    );
    let gate = probe();
    assert_eq!(gate.check(&fetcher, &external).await, Verdict::Unknown);
    assert_eq!(gate.check(&fetcher, &single_use).await, Verdict::Unknown);
    assert!(fetcher.requests().is_empty());
}

#[tokio::test]
async fn concurrency_and_whole_batch_deadline_are_bounded() {
    let fetcher = Arc::new(ScriptedFetcher {
        delay: Duration::from_millis(20),
        ..Default::default()
    });
    let gate = StreamProbe::new(ProbeConfig {
        concurrency: 2,
        timeout: Duration::from_millis(70),
        ..Default::default()
    });
    let cards: Vec<_> = (0..100)
        .map(|i| stream(&format!("/{i}"), Format::Mp4))
        .collect();
    let start = Instant::now();
    let result = gate.filter(fetcher.as_ref(), cards).await;
    assert_eq!(
        result.len(),
        100,
        "unprobed and inconclusive cards must ship"
    );
    assert!(start.elapsed() < Duration::from_millis(300));
    assert!(fetcher.peak.load(Ordering::SeqCst) <= 2);
    assert!(fetcher.requests().len() < 10);
}

#[tokio::test]
async fn byterange_playlists_probe_the_correct_file_offset() {
    let fetcher = ScriptedFetcher::default();
    fetcher.reply(
        "/master.m3u8",
        response(
            "/master.m3u8",
            200,
            "text/plain",
            b"#EXTM3U\n#EXT-X-BYTERANGE:999@12000\n#EXTINF:1,\nfile.mp4\n",
        ),
    );
    fetcher.reply(
        "/file.mp4",
        response("/file.mp4", 206, "video/mp4", b"\0\0\0\x18moofdata"),
    );
    assert_eq!(
        probe()
            .check(&fetcher, &stream("/master.m3u8", Format::Hls))
            .await,
        Verdict::Alive
    );
    assert_eq!(
        fetcher.requests()[1]
            .headers
            .get("range")
            .map(String::as_str),
        Some("bytes=12000-14047")
    );
}

#[tokio::test]
async fn empty_live_and_cyclic_playlists_remain_inconclusive() {
    for body in ["#EXTM3U\n", "#EXTM3U\nmaster.m3u8\n"] {
        let fetcher = ScriptedFetcher::default();
        fetcher.reply(
            "/master.m3u8",
            response("/master.m3u8", 200, "text/plain", body.as_bytes()),
        );
        assert_eq!(
            probe()
                .check(&fetcher, &stream("/master.m3u8", Format::Hls))
                .await,
            Verdict::Unknown
        );
    }
}

#[tokio::test]
async fn malformed_or_truncated_playlist_uris_and_dash_are_inconclusive() {
    for bytes in [
        b"#EXTM3U\n#EXT-X-BYTERANGE:12\nfile.ts\n#EXT-X-ENDLIST\n".as_slice(),
        b"#EXTM3U\n#EXT-X-BYTERANGE:12@18446744073709551615\nfile.ts\n#EXT-X-ENDLIST\n",
        b"<?xml version=\"1.0\"?><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\"></MPD>",
    ] {
        let fetcher = ScriptedFetcher::default();
        fetcher.reply(
            "/master",
            response("/master", 200, "application/octet-stream", bytes),
        );
        assert_eq!(
            probe()
                .check(&fetcher, &stream("/master", Format::Hls))
                .await,
            Verdict::Unknown
        );
    }
    let fetcher = ScriptedFetcher::default();
    let mut truncated = response(
        "/master",
        200,
        "text/plain",
        b"#EXTM3U\n#EXTINF:6,\nhttps://incomplete",
    );
    truncated.truncated = true;
    fetcher.reply("/master", truncated);
    assert_eq!(
        probe()
            .check(&fetcher, &stream("/master", Format::Hls))
            .await,
        Verdict::Unknown
    );
    assert_eq!(fetcher.requests().len(), 1);
}

#[tokio::test]
async fn cancellation_releases_coalesced_work_and_permits() {
    let fetcher = ScriptedFetcher {
        delay: Duration::from_millis(40),
        ..Default::default()
    };
    fetcher.reply(
        "/file",
        response("/file", 206, "video/mp4", b"\0\0\0\x18ftypisom"),
    );
    let gate = StreamProbe::new(ProbeConfig {
        concurrency: 1,
        ..Default::default()
    });
    let card = stream("/file", Format::Mp4);
    assert!(
        tokio::time::timeout(Duration::from_millis(5), gate.check(&fetcher, &card))
            .await
            .is_err()
    );
    let verdict = tokio::time::timeout(Duration::from_secs(1), gate.check(&fetcher, &card)).await;
    assert_eq!(verdict.ok(), Some(Verdict::Alive));
}
