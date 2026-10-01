#!/usr/bin/env python3
"""Resolve real providers and decode video/audio, without saving full media.

Requires a built target/debug/vsources, FFmpeg and TMDB credentials in env.
Reports omit signed URLs/headers; detailed process logs stay in --logs.
"""
import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
import threading
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
SERIES = {209867, 37854, 1396, 95479, 85937, 1429}
DEFAULTS = [209867, 37854, 27205, 1396, 1423191, 1204680, 1288445]
HOST_LOCKS = {}
HOST_LOCKS_LOCK = threading.Lock()

ANIME = {"allwish", "anibd", "anichan", "anidoor", "anikage", "anikoto", "anikototv", "animeflix", "animegg", "animekai", "animesuge", "animezey", "animotvslash", "hianime", "itachi", "2dhive", "nikastream", "reanime"}


def run(command, timeout, log, env):
    started = time.monotonic()
    with log.open("w") as errors:
        try:
            p = subprocess.run(command, capture_output=False, stdout=subprocess.PIPE, stderr=errors,
                               text=True, timeout=timeout, env=env, stdin=subprocess.DEVNULL)
            return p.returncode, p.stdout, round(time.monotonic() - started, 3)
        except subprocess.TimeoutExpired:
            return 124, "", round(time.monotonic() - started, 3)


def resolve(provider, media_id, args, env):
    command = [str(args.binary), "resolve", f"tmdb:{media_id}", "--provider", provider, "--json", "--timeout", str(args.source_timeout)]
    if args.english_dub:
        command.append("--english-dub")
    if args.fast:
        command.append("--fast")
    if media_id in SERIES:
        command += ["--kind", "series", "--season", str(args.season), "--episode", str(args.episode)]
    else:
        command += ["--kind", "movie"]
    log = args.logs / f"{provider}-{media_id}-resolve.log"
    code, output, elapsed = run(command, args.source_timeout + 40, log, env)
    if code:
        return [], {"status": "resolve_error", "exit": code, "resolve_seconds": elapsed}
    try:
        cards = json.loads(output)
    except json.JSONDecodeError:
        # Older CLI builds sent tracing warnings to stdout. Keep the audit
        # usable with those builds, but never interpret warnings as streams.
        match = re.search(r"(?m)^\[\s*$|(?m:^\[\])", output)
        if not match:
            return [], {"status": "invalid_json", "resolve_seconds": elapsed}
        cards = json.loads(output[match.start():])
    return cards, {"status": "resolved" if cards else "empty", "cards": len(cards), "resolve_seconds": elapsed}


def decode(provider, media_id, card, index, args, env):
    headers = card.get("meta", {}).get("request_headers", {})
    identity = json.dumps([card["url"], headers], sort_keys=True).encode()
    stamp = hashlib.sha256(identity).hexdigest()[:16]
    log = args.logs / f"{provider}-{media_id}-{index}-{stamp}.log"
    command = ["ffmpeg", "-hide_banner", "-nostdin", "-nostats", "-loglevel", "info",
               "-progress", "pipe:1", "-rw_timeout", "12000000", "-threads", "2",
               "-protocol_whitelist", "http,https,tcp,tls,crypto,data", "-analyzeduration", "10000000",
               "-probesize", "10000000"]
    if headers:
        command += ["-headers", "".join(f"{k}: {v}\r\n" for k, v in headers.items())]
    if card.get("format") == "hls":
        # Several real CDNs serve MPEG-TS at .jpg URLs. Keep network protocols
        # restricted while permitting those names in FFmpeg's HLS demuxer.
        command += ["-allowed_extensions", "ALL", "-allowed_segment_extensions", "ALL", "-extension_picky", "0"]
    selection = card.get("meta", {}).get("audio_selection")
    audio_index = selection["audio_index"] if selection else None
    if audio_index is not None and (not isinstance(audio_index, int) or isinstance(audio_index, bool) or audio_index < 0):
        raise ValueError("invalid required audio index")
    audio_map = f"0:a:{audio_index}" if audio_index is not None else "0:a:0?"
    command += ["-i", card["url"], "-t", str(args.seconds), "-map", "0:v:0", "-map", audio_map,
                "-vf", "scale=320:-2", "-threads", "2", "-filter_threads", "1", "-f", "null", "-"]
    host = urlsplit(card["url"]).hostname
    with HOST_LOCKS_LOCK:
        lock = HOST_LOCKS.setdefault(host, threading.Lock())
    with lock:
        code, progress, elapsed = run(command, 55, log, env)
        time.sleep(0.3)
    values = dict(line.split("=", 1) for line in progress.splitlines() if "=" in line)
    frames = int(values.get("frame", "0").strip() or 0)
    media_seconds = int(values.get("out_time_us", "0").strip() or 0) / 1_000_000
    tail = log.read_text(errors="replace")[-12000:]
    audio = re.findall(r"audio:([\d.]+)([A-Za-z]+)", tail)
    audio_decoded = bool(audio and float(audio[-1][0]) > 0)
    # A successful null muxer run requires actual decoding (no -c copy).
    passed = code == 0 and frames >= 24 and media_seconds >= args.seconds - 0.2 and audio_decoded
    errors = [line for line in tail.splitlines() if any(s in line.lower() for s in ("error", "failed", "invalid", "403", "404", "timed out"))]
    return {"status": "played" if passed else "decode_failed", "exit": code,
            "host": urlsplit(card["url"]).hostname, "stream_hash": stamp, "format": card.get("format"),
            "frames": frames, "media_seconds": round(media_seconds, 3), "audio_decoded": audio_decoded,
            "required_audio_selection": selection,
            "wall_seconds": elapsed, "log": str(log), "error_kind": summarize(errors)}


def summarize(errors):
    text = " ".join(errors).lower()
    for token, label in [("403 forbidden", "http_403"), ("404 not found", "http_404"), ("429 too many", "rate_limited"), ("timed out", "timeout"),
                         ("http error 426", "http_426"), ("5xx server", "upstream_5xx"), ("invalid data", "invalid_media"), ("input/output error", "io_error"),
                         ("failed to resolve", "dns_error")]:
        if token in text:
            return label
    return "decoder_error" if errors else None


def choices(provider, args):
    history = []
    for path in sorted((ROOT / "docs/audits").glob("2026-09-26-*.jsonl")):
        for line in path.read_text().splitlines():
            row = json.loads(line)
            if row.get("provider") == provider and "streams" in row:
                score = sum(s.get("verdict") == "alive" for s in row["streams"])
                history.append((score, len(row["streams"]), row["tmdb"]))
    ranked = [r[2] for r in sorted(history, reverse=True)]
    preferred = [209867, 37854, 95479, 85937, 1429] if provider in ANIME else [27205, 1204680, 1423191, 1396, 438631]
    supplied = json.loads(args.cases.read_text()).get(provider, []) if args.cases else []
    ordered = list(dict.fromkeys(([int(x) for x in args.media.split(",")] if args.media else supplied + ranked + preferred + DEFAULTS)))
    if provider in ANIME and not args.media:
        ordered = [x for x in ordered if x in SERIES and x != 1396]
    return ordered[:args.titles]


def verify(provider, args, env):
    attempts = []
    for media_id in choices(provider, args):
        cards, resolution = resolve(provider, media_id, args, env)
        result = {"tmdb": media_id, "season": args.season if media_id in SERIES else None,
                  "episode": args.episode if media_id in SERIES else None, **resolution, "decodes": []}
        attempts.append(result)
        # Prefer modest-resolution direct streams, then diversify host families.
        cards.sort(key=lambda c: (c.get("is_external", False), abs((c.get("meta", {}).get("resolution") or 1080) - 720)))
        unique, keys, hosts, deferred = [], set(), set(), []
        for card in cards:
            key = (card["url"], json.dumps(card.get("meta", {}).get("request_headers", {}), sort_keys=True))
            if key in keys or card.get("is_external"):
                continue
            keys.add(key)
            host = urlsplit(card["url"]).hostname
            if host in hosts:
                deferred.append(card)
            else:
                unique.append(card)
                hosts.add(host)
        for index, card in enumerate((unique + deferred)[:args.cards]):
            evidence = decode(provider, media_id, card, index, args, env)
            result["decodes"].append(evidence)
            if evidence["status"] == "played":
                return {"provider": provider, "verified": True, "checked_at": datetime.datetime.now(datetime.timezone.utc).isoformat(), "attempts": attempts}
    return {"provider": provider, "verified": False, "checked_at": datetime.datetime.now(datetime.timezone.utc).isoformat(), "attempts": attempts}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/vsources")
    parser.add_argument("--providers", help="comma-separated IDs; default all registered providers")
    parser.add_argument("--english-dub", action="store_true", help="resolve English spoken audio and honor embedded track selection")
    parser.add_argument("--fast", action="store_true", help="test the fast English-dub path (defaults to its three shortlisted providers)")
    parser.add_argument("--media", help="comma-separated TMDB IDs; series IDs listed in this script use S1E1")
    parser.add_argument("--season", type=int, default=1)
    parser.add_argument("--episode", type=int, default=1)
    parser.add_argument("--cases", type=Path, help="JSON mapping provider IDs to preferred TMDB IDs")
    parser.add_argument("--titles", type=int, default=3)
    parser.add_argument("--cards", type=int, default=4)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--source-timeout", type=int, default=35)
    parser.add_argument("--seconds", type=int, default=8)
    parser.add_argument("--logs", type=Path, default=Path("/tmp/vsources-playback-logs"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.fast and not args.english_dub:
        parser.error("--fast requires --english-dub")
    if not os.environ.get("TMDB_API_KEY") and not os.environ.get("TMDB_ACCESS_TOKEN"):
        parser.error("set TMDB_API_KEY or TMDB_ACCESS_TOKEN")
    args.logs.mkdir(parents=True, exist_ok=True, mode=0o700)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "RUST_LOG": "off", "LC_ALL": "C"}
    if args.providers:
        providers = args.providers.split(",")
    elif args.fast:
        providers = ["aniwaves", "reanime", "animekai"]
    else:
        catalog = subprocess.check_output([str(args.binary), "providers", "--json"], env=env, text=True)
        providers = [p["id"] for p in json.loads(catalog)]
    with args.binary.open("rb") as binary_file:
        binary_hash = hashlib.file_digest(binary_file, "sha256").hexdigest()
    with args.output.open("w") as report, concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        jobs = {pool.submit(verify, p, args, env): p for p in providers}
        for future in concurrent.futures.as_completed(jobs):
            provider = jobs[future]
            try:
                result = future.result()
            except Exception as exc:
                result = {"provider": provider, "verified": False, "harness_error": type(exc).__name__}
            result["binary_sha256"] = binary_hash
            result["configured_env"] = [name for name in ("TMDB_API_KEY", "TMDB_ACCESS_TOKEN", "PECKLE_FEBBOX_COOKIE", "MOVIEBOX_MOBILE_SIGNING_KEY") if env.get(name)]
            report.write(json.dumps(result) + "\n")
            report.flush()
            print(f"{provider}: {'PLAYED' if result['verified'] else 'unverified'}", flush=True)


if __name__ == "__main__":
    main()
