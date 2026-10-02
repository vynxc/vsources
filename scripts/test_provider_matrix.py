#!/usr/bin/env python3
"""Local-only audit regression tests; no provider network access."""
import argparse
import functools
import http.server
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest
import provider_matrix as audit

class AuditTests(unittest.TestCase):
    def test_case_manifest_has_twenty_unique_old_and_new_titles(self):
        cases=audit.validate_cases(json.loads((audit.ROOT/'docs/audits/provider-matrix-cases.json').read_text()))
        self.assertEqual(len(cases),20)
        self.assertEqual(len({c['case_id'] for c in cases}),20)

    def test_public_cards_exclude_signed_urls_and_header_values(self):
        card={'url':'https://cdn.example/file.mp4?token=SECRET','meta':{'request_headers':{'Cookie':'SECRET','Referer':'https://page.example/?token=SECRET'},'audio_selection':{'audio_index':1,'language':'En'}}}
        public=audit.public_card(card)
        self.assertNotIn('SECRET',json.dumps(public))
        self.assertEqual(public['header_names'],['Cookie','Referer'])
        self.assertEqual(public['audio_selection']['audio_index'],1)
        other={**card,'meta':{**card['meta'],'audio_selection':{'audio_index':0,'language':'En'}}}
        self.assertNotEqual(audit.fingerprint(card),audit.fingerprint(other))

    def test_headers_reject_line_injection_and_profiles_preserve_cookie_scope(self):
        with self.assertRaises(ValueError):audit.headers_of({'meta':{'request_headers':{'Referer':'value\r\nCookie: bad'}}})
        card={'url':'https://cdn.example/x','meta':{'request_headers':{'Cookie':'caller-cookie'}}}
        response={'observed_contexts':[{'url':'https://api.example/','headers':{'Cookie':'other-secret','Authorization':'Bearer bad','Referer':'https://embed.example/'}}]}
        profiles=audit.header_profiles(card,response,5)
        for _,headers in profiles:
            self.assertEqual(headers['Cookie'],'caller-cookie')
            self.assertNotIn('Authorization',headers)
            self.assertNotIn('other-secret',json.dumps(headers))

    def test_header_recoveries_do_not_qualify_for_environment_shortlist(self):
        cases=[{'case_id':'one','category':'movie'}]
        rows=[{'provider':'unfixed','case_id':'one','category':'movie','baseline_played':False,'header_recovered':True,'cold':{'ms':5}},
              {'provider':'verified','case_id':'one','category':'movie','baseline_played':True,'cold':{'ms':100}}]
        self.assertEqual(audit.recommendations(rows,cases)['movie']['providers'],['verified'])
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'env'
            self.assertFalse(audit.export_env({'complete':False},path))
            self.assertFalse(path.exists())

    def test_resume_keeps_new_events_and_does_not_resurrect_invalidated_rows(self):
        old={'provider':'fixed','case_id':'one','status':'empty'}
        other={'provider':'other','case_id':'one','status':'played'}
        new={'provider':'fixed','case_id':'two','status':'played'}
        with tempfile.TemporaryDirectory() as directory:
            events=Path(directory)/'events.jsonl'
            events.write_text(json.dumps({'event':'invalidate','providers':['fixed']})+'\n'+json.dumps(new)+'\n'+'{partial')
            restored=audit.restore_events(events,[old,other])
            self.assertEqual(restored,[other,new])

    def test_report_rejects_wrong_catalog_audio_and_cached_playback(self):
        cases=audit.validate_cases(json.loads((audit.ROOT/'docs/audits/provider-matrix-cases.json').read_text()))
        def row(provider,case,language='eng',warm='played'):
            return {'provider':provider,'case_id':case['case_id'],'category':case['category'],
                'english_dub_required':case['category'] in ('anime','anime_movie'),
                'binary_sha256':'a'*64,'checked_at':audit.now(),'status':'played','cold':{'ms':10,'http_requests':1},
                'warm':[],'warm_median_ms':1,'streams':[{'host':'cdn.example'}],
                'decodes':[{'status':'played','selected_audio_language':language}],
                'baseline_played':True,'header_recovered':False,'warm_playback':{'status':warm}}
        movie=next(case for case in cases if case['category']=='movie')
        anime=next(case for case in cases if case['category']=='anime')
        rows=[row('allwish',movie),row('animekai',anime,'jpn'),row('imdbplay',movie,warm='decode_failed')]
        with tempfile.TemporaryDirectory() as directory:
            args=argparse.Namespace(output=Path(directory),cases=audit.ROOT/'docs/audits/provider-matrix-cases.json',
                binary_hash='a'*64,source_timeout=35,decode_timeout=40,seconds=8,cards=2,workers=1,warm_repeats=3,
                english_dub=True,warm_playback=True,generated_env=Path(directory)/'env')
            catalog={'providers':[{'id':name,'label':name} for name in ('allwish','animekai','imdbplay')]}
            report=audit.build_report(args,rows,catalog,cases)
            self.assertEqual([item['status'] for item in report['rows']],['wrong_catalog','wrong_audio','cache_playback_failed'])
            self.assertEqual(report['recommendations']['movie']['providers'],[])
            self.assertEqual(report['recommendations']['anime']['providers'],[])
            self.assertTrue((Path(directory)/'results.csv').exists())

    @unittest.skipUnless(shutil.which('ffmpeg'),'FFmpeg required for decoding regression')
    def test_real_decode_requires_headers_and_audio_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            command=['ffmpeg','-hide_banner','-loglevel','error','-f','lavfi','-i','testsrc=duration=2:size=64x64:rate=24',
                '-f','lavfi','-i','sine=frequency=440:duration=2','-c:v','mpeg4','-c:a','aac','-movflags','+faststart',str(root/'sample.mp4')]
            subprocess.run(command,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
            class Guard(http.server.SimpleHTTPRequestHandler):
                def do_GET(self):
                    if self.headers.get('Referer')!='https://embed.example/':
                        self.send_error(403);return
                    super().do_GET()
                def log_message(self,*args):pass
            handler=functools.partial(Guard,directory=str(root))
            server=http.server.ThreadingHTTPServer(('127.0.0.1',0),handler)
            thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
            try:
                card={'url':f'http://127.0.0.1:{server.server_port}/sample.mp4','format':'mp4','meta':{}}
                args=argparse.Namespace(seconds=1.5,decode_timeout=10)
                missing=audit.decode(card,{},args,root/'missing.log')
                self.assertEqual(missing['error_kind'],'http_403')
                good=audit.decode(card,{'Referer':'https://embed.example/'},args,root/'good.log')
                self.assertEqual(good['status'],'played')
                self.assertTrue(good['audio_decoded'])
                self.assertIsNotNone(good['first_frame_ms'])
                card['meta']['audio_selection']={'audio_index':99,'language':'En'}
                wrong=audit.decode(card,{'Referer':'https://embed.example/'},args,root/'wrong.log')
                self.assertEqual(wrong['status'],'decode_failed')
                self.assertFalse(wrong['audio_decoded'])
            finally:server.shutdown();server.server_close();thread.join()

if __name__=='__main__':unittest.main()
