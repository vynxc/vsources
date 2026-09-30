//! Dubbed detection handlers (TS `handlers.ts` lines 3076-3127).

use crate::processors::Processor;
use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Dubbed handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("dubbed", r"\b(?:fan\s?dub)\b")
            .with_transform(Transform::Boolean)
            .with_remove()
            .with_skip_from_title(),
        Handler::new("dubbed", r"\bMULTi\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new("dubbed", r"\b(?:Fan.*)?(?:DUBBED|dublado|dubbing|DUBS?)\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new(
            "dubbed",
            r"\b(?:.*\bsub(?:s|bed)?\b)?(?:[ _\-\[(\.])?(dual|multi)(?:[ _\-\[(\.])?(?:audio)\b",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"\b(?:.*\bsub(s|bed)?\b)")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::Boolean)
        .with_remove(),
        Handler::new("dubbed", r"\b(?:DUBBED|dublado|dubbing|DUBS?)\b")
            .with_transform(Transform::Boolean),
        Handler::process_only("dubbed", Processor::DubbedFromLanguages),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Cases from the TypeScript corpus (`languages.test.ts`); only the
    /// `dubbed` field (this module's field) is asserted.
    #[test]
    fn dubbed_table_cases() {
        let cases: &[(&str, Option<bool>)] = &[
            // `dublado`/`dubbed`/`dub` tags.
            ("A Freira (2018) Dublado HD-TS 720p", Some(true)),
            ("Grimm S01E11 Dublado BR [ kickUploader ]", Some(true)),
            ("madagascar 720p hebrew dubbed.mkv", Some(true)),
            (
                "Inuyasha_TV+Finale+OVA+Film+CD+Manga+Other; dub jpn,chn,eng sub chs (2019-09-21)",
                Some(true),
            ),
            // `MULTi`.
            (
                "Joker.2019.MULTi.Bluray.1080p.Atmos.7.1.En.Fr.Sp.Pt-DDR[EtHD]",
                Some(true),
            ),
            (
                "Cowboy Bebop - 1080p BDrip Audio+sub MULTI (VF / VOSTFR)",
                Some(true),
            ),
            // `dual`/`multi` + `audio` with no sub tag in front.
            ("Berserk 01-25 [dual audio JP,EN] MKV", Some(true)),
            (
                "Inception 2010 1080p BRRIP[dual-audio][eng-hindi]",
                Some(true),
            ),
            // The validator rejects `Dual-Audio` behind `Multi-Subs`; the
            // process handler still fires from the languages set.
            (
                "FLCL S05.1080p HMAX WEB-DL DD2.0 H 264-VARYG (FLCL: Shoegaze Dual-Audio Multi-Subs)",
                Some(true),
            ),
            // Accented `Áudio` defeats the pattern; the languages set still
            // carries `dual audio` for the process handler.
            (
                "O Rei do Show 2018 Dual Áudio 4K UtraHD By.Luan.Harper",
                Some(true),
            ),
            (
                "Men in Black International (2019) 720p Korsub HDRip x264 ESub [Dual Line Audio] [Hindi English]",
                Some(true),
            ),
            (
                "Jumanji The Next Level (2019) 720p HDCAM Ads Blurred x264 Dual A",
                Some(true),
            ),
            (
                "[IceBlue] Naruto (Season 01) - [Multi-Dub][Multi-Sub][Dublado][HEVC 10Bits] 800p BD",
                Some(true),
            ),
            // `DUB`/`dubbing` glued to a word is not a tag.
            (
                "Star.Wars.Skeleton.Crew.Sezon01.PLDUB.480p.DSNP.WEB-DL.H264.DDP5.1-K83",
                None,
            ),
            ("Shrek_Forever_After_(2010)__3D_HSBS_(DubbingPL).mkv", None),
            ("The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1", None),
        ];
        for (title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.dubbed, *expected, "title: {title}");
        }
    }

    /// The process handler reads the languages set (owned by the earlier
    /// language sections): `multi audio`/`dual audio` set `dubbed`.
    #[test]
    fn dubbed_process_handler_reads_languages() {
        let flcl = parse_torrent_title(
            "FLCL S05.1080p HMAX WEB-DL DD2.0 H 264-VARYG (FLCL: Shoegaze Dual-Audio Multi-Subs)",
        );
        assert_eq!(
            flcl.languages,
            Some(vec!["multi subs".to_string(), "dual audio".to_string()])
        );
        assert_eq!(flcl.dubbed, Some(true));

        let rei = parse_torrent_title("O Rei do Show 2018 Dual Áudio 4K UtraHD By.Luan.Harper");
        assert_eq!(rei.languages, Some(vec!["dual audio".to_string()]));
        assert_eq!(rei.dubbed, Some(true));
    }
}
