#!/usr/bin/env python3
"""Rerunnable provider/title audit. Private persistent SDK pipes; public sanitized evidence.

Run: python3 scripts/provider_matrix.py --output docs/audits/2026-10-02-matrix
Resume: same command with --resume. Report: --report-only.
"""
from __future__ import annotations
import argparse
import concurrent.futures
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import select
import statistics
import subprocess
import threading
import time
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
ANIME_ONLY = {'aniwaves','allwish','anibd','anichan','anidoor','anikage','anikoto','anikototv','animeflix','animegg','animekai','animesuge','animezey','animotvslash','hianime','itachi','2dhive','nikastream','reanime'}
CATEGORIES = ('movie', 'series', 'anime', 'anime_movie')
HOST_LOCKS: dict[str, threading.Lock] = {}
LOCK = threading.Lock()
UA = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36'

def now(): return dt.datetime.now(dt.timezone.utc).isoformat()
def sha(value): return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()
def atomic_json(path, value): atomic_text(path, json.dumps(value, indent=2, ensure_ascii=False) + '\n')
def atomic_text(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(text)
    temporary.replace(path)

def restore_events(path, initial):
    latest={(row['provider'],row['case_id']):row for row in initial}
    if path.exists():
        for line in path.read_text().splitlines():
            try:row=json.loads(line)
            except json.JSONDecodeError:continue
            if row.get('event')=='invalidate':
                providers=set(row['providers'])
                latest={key:value for key,value in latest.items() if key[0] not in providers}
            elif 'provider' in row and 'case_id' in row:
                latest[row['provider'],row['case_id']]=row
    return list(latest.values())

def validate_cases(manifest):
    cases = manifest['cases']
    if len(cases)!=20:raise ValueError('exactly twenty cases are required')
    if len({case['case_id'] for case in cases}) != len(cases): raise ValueError('duplicate case IDs')
    for category in CATEGORIES:
        rows = [case for case in cases if case['category'] == category]
        if len(rows) != 5: raise ValueError(f'{category} needs exactly five cases')
        if {case['era'] for case in rows} != {'classic', 'recent'}: raise ValueError(f'{category} needs classic and recent cases')
    for case in cases:
        if not re.fullmatch(r'[a-z0-9][a-z0-9_-]*',case['case_id']):raise ValueError('invalid case filename key')
        if type(case['tmdb']) is not int or case['tmdb']<=0:raise ValueError('invalid TMDB identity')
        if case['kind'] not in ('movie', 'series'): raise ValueError('invalid media kind')
        if case['kind'] == 'series' and (type(case['season']) is not int or type(case['episode']) is not int or case['season']<0 or case['episode']<1): raise ValueError('series requires explicit season/episode')
    return cases

def classify(text):
    text = text.lower()
    for expression, label in [
        (r'403|forbidden', 'http_403'), (r'401|unauthorized','http_401'),
        (r'404|not found','http_404'), (r'429|too many requests','rate_limited'),
        (r'426|upgrade required','http_426'), (r'\b50[0234]\b','upstream_5xx'),
        (r'failed to resolve|name or service not known|nodename','dns_error'),
        (r'timed out|timeout','timeout'), (r'cloudflare|challenge','cloudflare'),
        (r'invalid data|not a valid|moov atom','invalid_media'), (r'input/output error','io_error'),
        (r'no stream|does not contain any stream|matches no streams','missing_tracks')]:
        if re.search(expression,text): return label
    return 'decoder_error' if text else None

def headers_of(card):
    headers = card.get('meta',{}).get('request_headers',{}) or {}
    if any('\r' in str(key)+str(value) or '\n' in str(key)+str(value) for key,value in headers.items()):
        raise ValueError('header injection')
    return dict(headers)

def fingerprint(card, headers=None):
    return sha([card['url'], headers if headers is not None else headers_of(card),card.get('meta',{}).get('audio_selection')])[:20]

def public_card(card):
    meta = card.get('meta', {})
    # Labels and error strings can contain tokens. Public reports use trusted case titles only.
    return {'stream_hash':fingerprint(card), 'host':urlsplit(card['url']).hostname,
        'format':card.get('format'), 'resolution':meta.get('resolution'), 'quality':meta.get('quality'),
        'external':card.get('is_external',False), 'header_names':sorted(headers_of(card)),
        'languages':meta.get('languages',[]), 'dubbed':meta.get('dubbed'), 'subbed':meta.get('subbed'),
        'audio_selection':meta.get('audio_selection'), 'stream_ttl':card.get('ttl')}

def header_profiles(card, response, maximum):
    original = headers_of(card)
    def changed(extra):
        value = dict(original)
        for name,content in extra.items():
            old = next((key for key in value if key.lower()==name.lower()),None)
            if old: del value[old]
            value[name] = content
        return value
    candidates = [('browser_user_agent', changed({'User-Agent':UA}))]
    referer = next((value for key,value in original.items() if key.lower()=='referer'),None)
    if referer:
        parts=urlsplit(referer)
        if parts.scheme in ('http','https') and parts.netloc:
            candidates.append(('origin_from_existing_referer',changed({'User-Agent':UA,'Origin':f'{parts.scheme}://{parts.netloc}'})))
    # Only observed public playback context headers are considered. Never copy an
    # API authorization header/cookie into another media host.
    contexts = response.get('observed_contexts',[])
    exact = [context for context in contexts if urlsplit(context['url']).hostname == urlsplit(card['url']).hostname]
    other = [context for context in contexts if context not in exact]
    for context in (exact + list(reversed(other)))[:10]:
        extra = {key:value for key,value in context.get('headers',{}).items() if key.lower() in ('referer','origin')}
        if extra: candidates.append(('observed_playback_context',changed({**extra,'User-Agent':UA})))
    base=response.get('base_url')
    if base:
        parts=urlsplit(base)
        origin=f'{parts.scheme}://{parts.netloc}'
        candidates.append(('provider_origin',changed({'User-Agent':UA,'Referer':origin+'/','Origin':origin})))
    seen={json.dumps(original,sort_keys=True)}
    result=[]
    for label,value in candidates:
        key=json.dumps(value,sort_keys=True)
        if key not in seen:
            result.append((label,value));seen.add(key)
    return result[:maximum]

def ffmpeg_command(card, headers, seconds):
    selection=card.get('meta',{}).get('audio_selection')
    index=selection.get('audio_index') if selection else None
    if index is not None and (type(index) is not int or index<0): raise ValueError('invalid required audio selection')
    command=['ffmpeg','-hide_banner','-nostdin','-nostats','-loglevel','info','-stats_period','0.1',
        '-progress','pipe:1','-rw_timeout','10000000','-threads','2',
        '-protocol_whitelist','http,https,tcp,tls,crypto,data','-analyzeduration','5000000','-probesize','5000000']
    if headers:
        command += ['-headers',''.join(f'{key}: {value}\r\n' for key,value in headers.items())]
    if card.get('format')=='hls':
        command += ['-allowed_extensions','ALL','-allowed_segment_extensions','ALL','-extension_picky','0']
    command += ['-i',card['url'],'-t',str(seconds),'-map','0:v:0','-map',f'0:a:{index}' if index is not None else '0:a:0',
        '-vf','scale=320:-2','-threads','2','-filter_threads','1','-f','null','-']
    return command

def decode(card, headers, args, log, seconds=None):
    seconds=seconds or args.seconds
    command=ffmpeg_command(card,headers,seconds)
    host=urlsplit(card['url']).hostname or 'unknown'
    with LOCK: host_lock=HOST_LOCKS.setdefault(host,threading.Lock())
    queued=time.monotonic()
    with host_lock:
        queue_ms=(time.monotonic()-queued)*1000
        started=time.monotonic();first=None;values={}
        log.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
        with log.open('w') as errors:
            os.chmod(log,0o600)
            process=subprocess.Popen(command,stdout=subprocess.PIPE,stderr=errors,text=True,bufsize=1,stdin=subprocess.DEVNULL)
            def read_progress():
                nonlocal first
                for line in process.stdout:
                    if '=' not in line: continue
                    key,value=line.strip().split('=',1);values[key]=value
                    if key=='frame' and int(value.strip() or 0)>0 and first is None:
                        first=(time.monotonic()-started)*1000
            reader=threading.Thread(target=read_progress,daemon=True);reader.start()
            try: code=process.wait(timeout=args.decode_timeout)
            except subprocess.TimeoutExpired:
                process.kill();process.wait();code=124
            reader.join(timeout=3)
            process.stdout.close()
        elapsed=(time.monotonic()-started)*1000
    tail=log.read_text(errors='replace')[-20000:]
    frames=int(values.get('frame','0').strip() or 0)
    duration=int(values.get('out_time_us','0').strip() or 0)/1_000_000
    audio=re.findall(r'audio:([\d.]+)([A-Za-z]+)',tail)
    audio_decoded=bool(audio and float(audio[-1][0])>0)
    minimum_frames=max(24,int(seconds*12))
    passed=code==0 and frames>=minimum_frames and duration>=seconds-.2 and audio_decoded
    # Language tags are evidence of metadata only, not speech recognition.
    audio_tags=re.findall(r'Stream #\d+:\d+(?:\[.*?\])?\(([^)]+)\): Audio:',tail)
    video=re.findall(r'Video: ([^,\n]+)',tail)
    selected_input=re.search(r'Stream #0:(\d+) -> #0:1',tail)
    input_section=tail.split('Stream mapping:')[0]
    input_languages={index:language for index,language in re.findall(r'Stream #0:(\d+)(?:\[.*?\])?\(([^)]+)\): Audio:',input_section)}
    selected_language=input_languages.get(selected_input.group(1)) if selected_input else None
    return {'status':'played' if passed else 'decode_failed','exit':code,'frames':frames,
        'media_seconds':round(duration,3),'audio_decoded':audio_decoded,'selected_audio_language':selected_language,'audio_tags':sorted(set(audio_tags)),
        'video_codecs':sorted(set(video)),'first_frame_ms':round(first,3) if first is not None else None,
        'decode_ms':round(elapsed,3),'host_queue_ms':round(queue_ms,3),
        'error_kind':None if passed else ('timeout' if code==124 else classify(tail)),
        'header_names':sorted(headers),'header_hash':sha(headers)[:20],
        'private_log_name':f'{getattr(args,"run_id","legacy")}/{log.name}'}

class Worker:
    def __init__(self,provider,args):
        self.provider=provider;self.args=args
        log=args.private/f'{provider}-worker.log'
        self.log=log.open('a');os.chmod(log,0o600)
        self.process=subprocess.Popen([str(args.binary),provider],cwd=ROOT,stdin=subprocess.PIPE,stdout=subprocess.PIPE,
            stderr=self.log,text=True,bufsize=1,env={**os.environ,'RUST_LOG':'off'})
    def request(self,case):
        request={**case,'timeout':self.args.source_timeout,'warm_repeats':self.args.warm_repeats,
            'english_dub':self.args.english_dub and case['category'] in ('anime','anime_movie')}
        self.process.stdin.write(json.dumps(request)+'\n');self.process.stdin.flush()
        deadline=time.monotonic()+self.args.source_timeout*(self.args.warm_repeats+1)+45
        if not select.select([self.process.stdout],[],[],max(0,deadline-time.monotonic()))[0]:
            self.close();raise TimeoutError('SDK worker budget expired')
        line=self.process.stdout.readline()
        if not line: raise RuntimeError('SDK worker stopped')
        return json.loads(line)
    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:self.process.kill();self.process.wait()
        for pipe in (self.process.stdin,self.process.stdout):
            if pipe:pipe.close()
        self.log.close()

def audit_case(worker,provider,case,args):
    started=time.monotonic();response=worker.request(case)
    raw_log=args.private/f'{provider}-{case["case_id"]}-response.json'
    raw_log.write_text(json.dumps(response));os.chmod(raw_log,0o600)
    cold=dict(response['cold']);error=cold.pop('error',None);cold['error_kind']=classify(error or '') if error else None
    warm=[]
    for sample in response.get('warm',[]):
        sample=dict(sample);error=sample.pop('error',None)
        sample['error_kind']=classify(error or '') if error else None
        sample['cache_hit']=sample['http_requests']==0 and sample['status'] in ('resolved','empty')
        warm.append(sample)
    cards=response.get('streams',[])
    row={'provider':provider,'case_id':case['case_id'],'category':case['category'],'english_dub_required':args.english_dub and case['category'] in ('anime','anime_movie'),
        'checked_at':now(),'binary_sha256':args.binary_hash,'metadata_ms':response.get('metadata_ms'),
        'cold':cold,'warm':warm,'warm_median_ms':statistics.median([sample['ms'] for sample in warm]) if warm else None,
        'streams':[public_card(card) for card in cards],'decodes':[],'status':cold['status'],'baseline_played':False,
        'header_recovered':False,'warm_playback':None}
    candidates=[];seen=set();hosts=set();deferred=[]
    for card in sorted(cards,key=lambda card:(card.get('is_external',False),abs((card.get('meta',{}).get('resolution') or 1080)-720))):
        key=fingerprint(card)
        if key in seen or card.get('is_external'):continue
        seen.add(key);host=urlsplit(card['url']).hostname
        if host in hosts:deferred.append(card)
        else:candidates.append(card);hosts.add(host)
    playable=None;playable_headers=None
    failed_cards=[]
    for index,card in enumerate((candidates+deferred)[:args.cards]):
        headers=headers_of(card)
        evidence=decode(card,headers,args,args.private/f'{provider}-{case["case_id"]}-{index}-baseline.log')
        evidence.update({'stream_hash':fingerprint(card),'host':urlsplit(card['url']).hostname,'profile':'sdk_headers'})
        row['decodes'].append(evidence)
        if evidence['status']=='played':
            row['baseline_played']=True;playable=card;playable_headers=headers;break
        failed_cards.append((index,card,headers,evidence))
    # Try alternate SDK cards before experimenting with headers.
    if not playable:
        for index,card,headers,evidence in failed_cards:
            if evidence['error_kind'] not in ('http_403','http_401','invalid_media','io_error'):continue
            for profile,modified in header_profiles(card,response,args.header_retries):
                fix=decode(card,modified,args,args.private/f'{provider}-{case["case_id"]}-{index}-{len(row["decodes"])}-headers.log')
                fix.update({'stream_hash':fingerprint(card),'host':urlsplit(card['url']).hostname,'profile':profile,
                    'changed_header_names':sorted(key for key,value in modified.items() if not any(old.lower()==key.lower() and previous==value for old,previous in headers.items()))})
                row['decodes'].append(fix)
                if fix['status']=='played':
                    row['header_recovered']=True;playable=card;playable_headers=modified
                    patch={'provider':provider,'case_id':case['case_id'],'host':urlsplit(card['url']).hostname,
                        'headers':modified,'original_headers':headers,'observed_contexts':response.get('observed_contexts',[]),'stream':card}
                    path=args.private/f'{provider}-{case["case_id"]}-header-recovery.json';path.write_text(json.dumps(patch));os.chmod(path,0o600)
                    break
            if playable:break
    if playable:
        row['status']='played' if row['baseline_played'] else 'header_recovered'
        if args.warm_playback:
            # Immediately retained signed URL. This measures player/CDN startup;
            # source cache latency is measured independently above, with request counts.
            row['warm_playback']=decode(playable,playable_headers,args,args.private/f'{provider}-{case["case_id"]}-warm.log')
            row['warm_playback']['same_stream_hash']=fingerprint(playable)
        success=next(item for item in row['decodes'] if item['status']=='played')
        prior=row['decodes'][:row['decodes'].index(success)]
        row['cold_to_first_frame_ms']=cold['ms']+(response.get('metadata_ms') or 0)+sum(item['decode_ms']+item['host_queue_ms'] for item in prior)+success['first_frame_ms']+success['host_queue_ms']
        row['warm_to_first_frame_ms']=(row['warm_median_ms'] or 0)+(row['warm_playback'].get('first_frame_ms') or 0) if row['warm_playback'] and row['warm_playback']['status']=='played' else None
    elif row['decodes']:row['status']='decode_failed'
    row['attempt_ms']=round((time.monotonic()-started)*1000,3)
    return row

def summarize(rows,providers,cases):
    summary=[]
    for provider in providers:
        items=[row for row in rows if row['provider']==provider['id']]
        good=[row for row in items if row['baseline_played']]
        summary.append({'id':provider['id'],'label':provider['label'],'attempted':len(items),'expected':len(cases),
            'played':len(good),'header_recovered':sum(row['header_recovered'] for row in items),
            'cold_median_ms':statistics.median([row['cold']['ms'] for row in items]) if items else None,
            'warm_median_ms':statistics.median([row['warm_median_ms'] for row in items if row.get('warm_median_ms') is not None]) if any(row.get('warm_median_ms') is not None for row in items) else None,
            'categories':{category:{'attempted':sum(row['category']==category for row in items),'played':sum(row['category']==category for row in good)} for category in CATEGORIES}})
    return summary

def recommendations(rows,cases):
    # Only SDK-header playback passes qualify. Header experiments remain excluded
    # until the SDK is corrected and an unmodified fresh resolve passes.
    picks={}
    for category in CATEGORIES:
        eligible=[row for row in rows if row['category']==category and row['baseline_played'] and row.get('cache_playback_valid',True)]
        uncovered={case['case_id'] for case in cases if case['category']==category}
        selected=[]
        while uncovered:
            choices=[]
            for provider in {row['provider'] for row in eligible}-set(selected):
                group=[row for row in eligible if row['provider']==provider]
                coverage={row['case_id'] for row in group}&uncovered
                speed=statistics.median([row.get('cold_to_first_frame_ms') or row['cold']['ms'] for row in group])
                choices.append((len(coverage),len(group),-speed,provider,coverage))
            if not choices:break
            best=max(choices)
            if not best[0]:break
            selected.append(best[3]);uncovered-=best[4]
        picks[category]={'providers':selected,'uncovered_cases':sorted(uncovered)}
    return picks

def fast_dub_compatibility(rows):
    candidates=[]
    for provider in ('aniwaves','reanime','animekai'):
        good=[row for row in rows if row['provider']==provider and row['category'] in ('anime','anime_movie')
            and row['baseline_played'] and row.get('cache_playback_valid',True)]
        if good:candidates.append((statistics.median(row.get('cold_to_first_frame_ms') or row['cold']['ms'] for row in good),provider))
    return [provider for _,provider in sorted(candidates)[:2]]

def export_env(report,path):
    if not report['complete']:return False
    picks=report['recommendations'];ids=list(dict.fromkeys([provider for category in CATEGORIES for provider in picks[category]['providers']]
        + report.get('fast_dub_compatible',[])))
    if any(not re.fullmatch(r'[a-z0-9][a-z0-9_-]*',provider) for provider in ids):raise ValueError('unsafe provider ID in environment export')
    lines=['# Generated from actual SDK-header video+audio playback passes.',f'# Audit: {report["updated_at"]}',
        '# Load credentials from .env first; this file contains no credentials.',
        '# Apply global allowlist with: set -a; source .env; source .env.generated; set +a',
        'VSOURCES_PROVIDERS='+(','.join(ids) if ids else '__none__'),'VSOURCES_AUDIT_FAST_DUB_PROVIDERS='+','.join(report.get('fast_dub_compatible',[])),
        'VSOURCES_AUDIT_CASES='+__import__('shlex').quote(report.get('case_manifest','docs/audits/provider-matrix-cases.json'))]
    for category in CATEGORIES:
        lines += [f'# {category}: uncovered cases: '+(','.join(picks[category]['uncovered_cases']) or 'none'),
            f'VSOURCES_AUDIT_{category.upper()}_PROVIDERS='+','.join(picks[category]['providers'])]
    atomic_text(path,'\n'.join(lines)+'\n');return True

def export_csv(report,path):
    import csv
    names={case['case_id']:case for case in report['cases']}
    fields=['provider','case_id','title','category','tmdb','season','episode','status','checked_at','binary_sha256',
        'cold_ms','metadata_ms','cold_http_requests','warm_median_ms','warm_http_requests','cold_to_first_frame_ms',
        'warm_to_first_frame_ms','attempt_ms','cards','decode_attempts','sdk_playback','cache_playback_valid',
        'selected_audio_language','header_recovered']
    with path.open('w',newline='') as output:
        writer=csv.DictWriter(output,fieldnames=fields,lineterminator="\n");writer.writeheader()
        for row in report['rows']:
            case=names[row['case_id']]
            title=case['title']
            if title.startswith(('=','+','-','@')):title="'"+title
            writer.writerow({'provider':row['provider'],'case_id':row['case_id'],'title':title,
                'category':row['category'],'tmdb':case['tmdb'],'season':case['season'],'episode':case['episode'],
                'status':row['status'],'checked_at':row['checked_at'],'binary_sha256':row['binary_sha256'],
                'cold_ms':row['cold']['ms'],'metadata_ms':row.get('metadata_ms'),'cold_http_requests':row['cold']['http_requests'],
                'warm_median_ms':row.get('warm_median_ms'),'warm_http_requests':','.join(str(x['http_requests']) for x in row['warm']),
                'cold_to_first_frame_ms':row.get('cold_to_first_frame_ms'),'warm_to_first_frame_ms':row.get('warm_to_first_frame_ms'),
                'attempt_ms':row.get('attempt_ms'),'cards':len(row['streams']),'decode_attempts':len(row['decodes']),
                'sdk_playback':row['baseline_played'],'cache_playback_valid':row.get('cache_playback_valid'),
                'selected_audio_language':row.get('selected_audio_language'),'header_recovered':row['header_recovered']})

def build_report(args,rows,catalog,cases,history=None):
    expected=len(catalog['providers'])*len(cases)
    for row in rows:
        for sample in [row['cold'],*row.get('warm',[])]:
            if sample.get('error_kind')=='decoder_error':sample['error_kind']='scrape_error'
        row['request_scope_matches'] = not (row['provider'] in ANIME_ONLY and row['category'] in ('movie','series'))
        if not row['request_scope_matches'] and row.get('streams'):
            row.setdefault('transport_status',row['status'])
            row.setdefault('transport_played',row['baseline_played'])
            row['status']='wrong_catalog'
            row['baseline_played']=False
            row['identity_status']='out_of_scope'
        else:
            row.setdefault('identity_status','provider_identity_not_independently_viewed')
        selected=next((item for item in row.get('decodes',[]) if item['status']=='played'),None)
        if selected and row.get('english_dub_required'):
            language=selected.get('selected_audio_language')
            if language is None and len(selected.get('audio_tags',[]))==1:language=selected['audio_tags'][0]
            row['selected_audio_language']=language
            if language and language.lower() not in ('eng','en','en-us','en-gb','und','unknown'):
                row.setdefault('transport_status',row['status'])
                row.setdefault('transport_played',row['baseline_played'])
                row['status']='wrong_audio';row['baseline_played']=False
        warm=row.get('warm_playback')
        row['cache_playback_valid'] = warm is None or warm['status']=='played'
        if row['baseline_played'] and not row['cache_playback_valid']:
            row['status']='cache_playback_failed'
    report={'schema_version':1,'updated_at':now(),'case_manifest':str(args.cases.resolve()),'manifest_sha256':hashlib.sha256(args.cases.read_bytes()).hexdigest(),'complete':len(rows)==expected,'expected':expected,'finished':len(rows),
        'binary_sha256':args.binary_hash,'binary_builds':sorted({row['binary_sha256'] for row in rows}),'configured_env':catalog.get('configured_env',[]),'providers':catalog['providers'],'cases':cases,
        'settings':{'source_timeout':args.source_timeout,'decode_timeout':args.decode_timeout,'seconds':args.seconds,'cards':args.cards,
            'workers':args.workers,'warm_repeats':args.warm_repeats,'english_dub':args.english_dub,'warm_playback':args.warm_playback},
        'methodology':['First resolve per title in a persistent provider process; shared upstream caches may already be warm from preceding titles.',
            'HTTP request counts refer to Fetcher request/probe invocations; transport redirects and internal host denial caches are not separately counted.',
            'Warm calls immediately reuse the same CachedSource instance. HTTP request counts distinguish cache reuse from retry or negative caching.',
            'FFmpeg progress reports bound first-frame timing to the first positive progress sample; this is headless decode, not device rendering.',
            'Playback requires video frames, near-full requested duration, decoded audio, and process exit zero.',
            'Known anime-only catalogs returning media for non-anime requests are marked wrong_catalog and excluded from recommendations. Transport decoding evidence is retained.',
            'Audio language tags and SDK dub labels are metadata evidence; spoken language is not independently transcribed in this matrix.',
            'At most the configured card count is sampled, stopping at first playable card. Unexamined cards are not classified as working.',
            'Combined first-frame paths are sums of separately measured metadata, source resolve, preceding failed decode attempts and successful startup.',
            'Header recovery is a controlled diagnostic on the same URL. Recovered results do not qualify for the generated environment until fixed in the SDK.',
            'Repeat playback starts a fresh FFmpeg process with the selected cached signed URL; its player/CDN timing is distinct from immediate source-cache latency.',
            'Each playback host is decoded serially; host queue time is recorded separately. Unsupported categories are attempted, not silently skipped.'],
        'fast_dub_compatible':fast_dub_compatibility(rows),'rows':rows,'history':history or [],'summary':summarize(rows,catalog['providers'],cases),'recommendations':recommendations(rows,cases)}
    atomic_json(args.output/'report.json',report)
    export_csv(report,args.output/'results.csv')
    template=(ROOT/'scripts/provider_matrix_report.html').read_text()
    # Script-safe escaping also keeps this usable as a standalone offline file.
    embedded=json.dumps(report,ensure_ascii=False).replace('<','\\u003c').replace('>','\\u003e').replace('&','\\u0026')
    atomic_text(args.output/'index.html',template.replace('__AUDIT_DATA__',embedded))
    export_env(report,args.generated_env)
    return report

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cases',type=Path,default=Path(os.environ.get('VSOURCES_AUDIT_CASES',str(ROOT/'docs/audits/provider-matrix-cases.json'))))
    parser.add_argument('--output',type=Path,default=ROOT/'docs/audits/2026-10-02-matrix')
    parser.add_argument('--private',type=Path,default=Path('/tmp/vsources-provider-matrix-20261002'))
    parser.add_argument('--binary',type=Path,default=ROOT/'target/debug/examples/matrix_worker')
    parser.add_argument('--generated-env',type=Path,default=ROOT/'.env.generated')
    parser.add_argument('--providers',help='comma-separated subset (full run defaults to all 48)')
    parser.add_argument('--resume',action='store_true')
    parser.add_argument('--rerun',help='rerun provider IDs, retaining previous rows in history')
    parser.add_argument('--report-only',action='store_true')
    parser.add_argument('--no-build',action='store_true')
    parser.add_argument('--english-dub',action=argparse.BooleanOptionalAction,default=True)
    parser.add_argument('--warm-playback',action=argparse.BooleanOptionalAction,default=True)
    parser.add_argument('--workers',type=int,default=6)
    parser.add_argument('--cards',type=int,default=2)
    parser.add_argument('--seconds',type=int,default=8)
    parser.add_argument('--source-timeout',type=int,default=35)
    parser.add_argument('--decode-timeout',type=int,default=40)
    parser.add_argument('--warm-repeats',type=int,default=3)
    parser.add_argument('--header-retries',type=int,default=3)
    args=parser.parse_args()
    if min(args.workers,args.cards,args.seconds,args.source_timeout,args.decode_timeout,args.warm_repeats)<1:parser.error('budgets and sample counts must be positive')
    args.output=args.output.resolve();args.private_root=args.private.resolve();args.binary=args.binary.resolve()
    args.run_id=dt.datetime.now(dt.timezone.utc).strftime('%Y%m%dT%H%M%S')+'-'+str(os.getpid())
    args.private=args.private_root/'runs'/args.run_id
    args.private.mkdir(parents=True,exist_ok=True,mode=0o700);os.chmod(args.private,0o700)
    cases=validate_cases(json.loads(args.cases.read_text()))
    if not args.no_build and not args.report_only:
        print('Building persistent SDK worker…',flush=True)
        with (args.private/'build.log').open('w') as log:
            subprocess.run(['cargo','build','-p','vsources','--example','matrix_worker'],cwd=ROOT,stdout=log,stderr=log,check=True)
    with args.binary.open('rb') as binary:args.binary_hash=hashlib.file_digest(binary,'sha256').hexdigest()
    if not args.report_only:
        # Every process in a run uses exactly the binary whose hash is recorded,
        # even if a later SDK repair rebuilds target/debug while the run continues.
        import shutil
        frozen=args.private_root/'binaries'/f'matrix-worker-{args.binary_hash[:16]}'
        frozen.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
        if not frozen.exists():shutil.copy2(args.binary,frozen)
        args.binary=frozen
    if args.report_only:
        saved=json.loads((args.output/'report.json').read_text())
        for key,value in saved['settings'].items():setattr(args,key,value)
        build_report(args,saved['rows'],{'providers':saved['providers'],'configured_env':saved['configured_env']},cases,saved.get('history'));return
    catalog=json.loads(subprocess.check_output([str(args.binary),'--catalog'],cwd=ROOT,env={**os.environ,'RUST_LOG':'off'},text=True))
    if args.providers:
        wanted=set(args.providers.split(','));catalog['providers']=[provider for provider in catalog['providers'] if provider['id'] in wanted]
        if {provider['id'] for provider in catalog['providers']}!=wanted:parser.error('unknown provider in subset')
    rows=[];history=[]
    report_file=args.output/'report.json'
    if args.resume or args.rerun:
        if report_file.exists():
            saved=json.loads(report_file.read_text());rows=saved['rows'];history=saved.get('history',[])
            rows=restore_events(args.output/'results.jsonl',rows)
            if args.providers and {provider['id'] for provider in saved['providers']} != {provider['id'] for provider in catalog['providers']}:
                parser.error('provider subset changed; use --rerun to preserve the full report or a new output directory')
            if saved['cases']!=cases:parser.error('case manifest changed; use a new output directory')
            if saved['settings']['english_dub']!=args.english_dub:parser.error('audio policy changed; use a new output directory')
        if args.rerun:
            rerun=set(args.rerun.split(','))
            with (args.output/'results.jsonl').open('a') as events:
                events.write(json.dumps({'event':'invalidate','providers':sorted(rerun),'requested_at':now()})+'\n')
            history.extend(row for row in rows if row['provider'] in rerun);rows=[row for row in rows if row['provider'] not in rerun]
    elif report_file.exists():parser.error('output exists; use --resume or a new output directory')
    catalog_ids={provider['id'] for provider in catalog['providers']}
    rows=[row for row in rows if row['provider'] in catalog_ids]
    completed={(row['provider'],row['case_id']) for row in rows}
    build_report(args,rows,catalog,cases,history)
    results=__import__('queue').Queue()
    def audit_provider(provider):
        worker=None
        try:
            for case in cases:
                if (provider,case['case_id']) in completed:continue
                try:
                    if worker is None:worker=Worker(provider,args)
                    row=audit_case(worker,provider,case,args)
                except Exception as error:
                    if worker:worker.close();worker=None
                    row={'provider':provider,'case_id':case['case_id'],'category':case['category'],'checked_at':now(),
                        'binary_sha256':args.binary_hash,'status':'harness_error','harness_error':type(error).__name__,
                        'cold':{'status':'harness_error','ms':0,'http_requests':0,'cards':0},'warm':[],
                        'warm_median_ms':None,'streams':[],'decodes':[],'baseline_played':False,'header_recovered':False,'warm_playback':None}
                results.put(row)
        finally:
            if worker:worker.close()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures=[pool.submit(audit_provider,provider['id']) for provider in catalog['providers']]
        last_report=time.monotonic()
        while any(not future.done() for future in futures) or not results.empty():
            try:row=results.get(timeout=1)
            except __import__('queue').Empty:continue
            rows.append(row)
            with (args.output/'results.jsonl').open('a') as output:output.write(json.dumps(row)+'\n')
            print(f'{len(rows)}/{len(catalog["providers"])*len(cases)} {row["provider"]:14} {row["case_id"]:20} {row["status"]:18} cold={row["cold"]["ms"]:.0f}ms warm={row.get("warm_median_ms")}',flush=True)
            if time.monotonic()-last_report>15:
                build_report(args,rows,catalog,cases,history);last_report=time.monotonic()
        for future in futures:future.result()
    report=build_report(args,rows,catalog,cases,history)
    print(f'Complete: {report["finished"]} checks; {sum(row["baseline_played"] for row in rows)} SDK-header playback passes. Report: {args.output/"index.html"}',flush=True)

if __name__=='__main__':main()
