//! Network handlers (TS `handlers.ts` lines 3190-3413).

use crate::transforms::Transform;
use crate::types::Handler;

/// Network handlers in original order.
// A verbatim data table; its length mirrors the TypeScript source section,
// and splitting it would obscure the 1:1 ordering that parsing relies on.
#[allow(clippy::too_many_lines)]
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("network", r"\b(?:ATVP?|APTV)\b|\bApple.?TV\+?")
            .with_transform(Transform::Value("Apple TV".to_string()))
            .with_remove(),
        Handler::new("network", r"\bAMZN\b|(?<=\W{2})\bAmazon(?:.?HD)?(?=\W{2})")
            .with_transform(Transform::Value("Prime Video".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\b(?:NF|Netflix)\b")
            .with_transform(Transform::Value("Netflix".to_string()))
            .with_remove(),
        Handler::new("network", r"\bNICK(?:elodeon)?\b")
            .with_transform(Transform::Value("Nickelodeon".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\bDSNY?P?\b|(?<=\W{2})\bDisney\+?(?=\W{2})")
            .with_transform(Transform::Value("Disney+".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\bSHO\b|(?<=\W{2})\bSHOWTIME(?=\W{2})")
            .with_transform(Transform::Value("Showtime".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\b(?:SKST|SKYSHO(?:WTIME)?)\b")
            .with_transform(Transform::Value("SkyShowtime".to_string()))
            .with_remove(),
        Handler::new("network", r"\b(?:PMNT|PMTP)\b|\bParamount(?:\+|.?Plus)")
            .with_transform(Transform::Value("Paramount+".to_string()))
            .with_remove(),
        Handler::new("network", r"\bPCOK\b|(?<=\W{2})\bPeacock(?=\W{2})")
            .with_transform(Transform::Value("Peacock".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\bCRAV\b|(?<=\W{2})\bCRAVE(?=\W{2})")
            .with_transform(Transform::Value("Crave".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\bBCORE\b|(?<=\W{2})\bCORE(?=\W{2})")
            .with_transform(Transform::Value("Sony Pictures Core".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\b(HMAX|HBOM?(?:ax)?)\b")
            .with_transform(Transform::Value("HBO Max".to_string()))
            .with_remove(),
        Handler::new("network", r"(?<=\W{2})MAX\b")
            .with_transform(Transform::Value("HBO Max".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"(?<=\W{2})STAN\b")
            .with_transform(Transform::Value("Stan".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\bitunes\b")
            .with_transform(Transform::Value("iTunes".to_string()))
            .with_remove(),
        Handler::new_case_sensitive("network", r"\biT\b")
            .with_transform(Transform::Value("iTunes".to_string()))
            .with_remove(),
        Handler::new("network", r"\bHULU\b")
            .with_transform(Transform::Value("Hulu".to_string()))
            .with_remove(),
        Handler::new("network", r"\bCBS\b")
            .with_transform(Transform::Value("CBS".to_string()))
            .with_remove(),
        Handler::new("network", r"\bNBC\b")
            .with_transform(Transform::Value("NBC".to_string()))
            .with_remove(),
        Handler::new("network", r"\bAMC\b")
            .with_transform(Transform::Value("AMC".to_string()))
            .with_remove(),
        Handler::new("network", r"\bPBS\b")
            .with_transform(Transform::Value("PBS".to_string()))
            .with_remove(),
        Handler::new("network", r"\b(Crunchyroll|CR)\b")
            .with_transform(Transform::Value("Crunchyroll".to_string()))
            .with_remove(),
        Handler::new_case_sensitive("network", r"\bVICE\b")
            .with_transform(Transform::Value("VICE".to_string()))
            .with_remove(),
        Handler::new("network", r"\bSony\b")
            .with_transform(Transform::Value("Sony".to_string()))
            .with_remove(),
        Handler::new("network", r"\bHallmark\b")
            .with_transform(Transform::Value("Hallmark".to_string()))
            .with_remove(),
        Handler::new("network", r"\bAdult.?Swim\b")
            .with_transform(Transform::Value("Adult Swim".to_string()))
            .with_remove(),
        Handler::new("network", r"\b(?:Animal.?Planet|ANPL)\b")
            .with_transform(Transform::Value("Animal Planet".to_string()))
            .with_remove(),
        Handler::new("network", r"\bCartoon.?Network(?:.TOONAMI.BROADCAST)?\b")
            .with_transform(Transform::Value("Cartoon Network".to_string()))
            .with_remove(),
        Handler::new("network", r"\bCRIT\b")
            .with_transform(Transform::Value("Criterion Channel".to_string()))
            .with_remove(),
        Handler::new("network", r"(?<=\W{2})\bG?PLAY(?=\W{2})")
            .with_transform(Transform::Value("Google TV".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"(?<=\W{2})\b(?:MA|YKW)(?=\W{2})")
            .with_transform(Transform::Value("Movies Anywhere".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("network", r"\bROKU\b")
            .with_transform(Transform::Value("The Roku Channel".to_string()))
            .with_remove(),
        Handler::new("network", r"\bDCU\b")
            .with_transform(Transform::Value("DC Universe".to_string()))
            .with_remove(),
        Handler::new("network", r"\bSYFY\b")
            .with_transform(Transform::Value("SYFY".to_string()))
            .with_remove(),
        Handler::new("network", r"(?<=\W{2})\b(?:MGM[P+]|EPIX)")
            .with_transform(Transform::Value("MGM+".to_string()))
            .with_remove()
            .with_skip_if_first(),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Table-driven cases ported from `network.test.ts`, asserting only the
    /// `network` field owned by this module.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn network_matches_ported_corpus() {
        let cases: &[(&str, Option<&str>)] = &[
            // Netflix via `NF`.
            (
                "Extraction.2020.720p.NF.WEB-DL.Dual.Atmos.5.1.x264-BonsaiHD",
                Some("Netflix"),
            ),
            (
                "Guilty (2020) NF Original 720p WEBRip [Hindi + English] AAC DD-5.1 ESub x264 - Shadow.mkv",
                Some("Netflix"),
            ),
            (
                "Childs.Play.1988.NFOFiX.PROPER.REPACK.DVDRip.XviD-zXx",
                None,
            ),
            (
                "The.Bear.S03.COMPLETE.1080p.HULU.WEB.H264-SuccessfulCrab[TGx]",
                Some("Hulu"),
            ),
            (
                "Futurama.S08E03.How.the.West.Was.1010001.1080p.HULU.WEB-DL.DDP5.1.H.264-FLUX.mkv",
                Some("Hulu"),
            ),
            // `Amazon` as a title word must not match.
            (
                "Primal Survivor Escape The Amazon S06E05 720p HDTV x264-CBFM EZTV",
                None,
            ),
            // Prime Video after an episode title.
            (
                "Fallout.S01E01.The.End.AMZN.WEB-DL.AAC2.0.H.264-BTW",
                Some("Prime Video"),
            ),
            (
                "Amazon.Queen.2021.720p.AMZN.WEBRip.800MB.x264-GalaxyRG",
                Some("Prime Video"),
            ),
            (
                "Law and Order S05E20 Bad Faith 720p Amazon WEB-DL DD 2 0 H 264-TrollHD[TGx]",
                Some("Prime Video"),
            ),
            (
                "Tron.Ares.2025.2160p.iTunes.WEB-DL.DDP5.1.Atmos.DV.HDR.H.265-BYNDR.mkv",
                Some("iTunes"),
            ),
            // Case-sensitive `iT` abbreviation.
            (
                "Tron.Ares.2025.2160p.iT.WEB-DL.DDP5.1.Atmos.DV.HDR.H.265-BYNDR.mkv",
                Some("iTunes"),
            ),
            // Crunchyroll via `CR`.
            (
                "[Yameii] SPY x FAMILY - S03E11 [English Dub] [CR WEB-DL 1080p] [195797EF] (SPY x FAMILY Season 3 | S3)",
                Some("Crunchyroll"),
            ),
            // `NICK` inside a title word must not match Nickelodeon.
            (
                "Mike.And.Nick.And.Nick.And.Alice.2026.2160p.DSNP.WEB.DL.DDP5.1.Atmos.DV.HDR.H.265.FLUX.mkv",
                Some("Disney+"),
            ),
            (
                "Family.Guy.S18E04.Disney's.The.Reboot.1080p.HULU.WEB-DL.DD+5.1.H.264-CtrlHD",
                Some("Hulu"),
            ),
            (
                "From.S01E07.All.Good.Things.540p.PMTP.WEB-DL.AAC2.0.H.264-lll",
                Some("Paramount+"),
            ),
            (
                "The.Neighborhood.S04.1080p.Paramount+.WEB-DL.DDP.5.1.H.264-CHDWEB",
                Some("Paramount+"),
            ),
            (
                "Poker.Face.S01E01.1080p.PCOK.WEB-DL.DDP5.1.H.264-NTb",
                Some("Peacock"),
            ),
            (
                "The.Bay.S01.1080p.Peacock.WEB-DL.AAC.2.0.H.264-CHDWEB",
                Some("Peacock"),
            ),
            // `Peacock` as a title word must not match.
            ("Peacock.2024.1080p.BluRay.x264-KNiVES", None),
            (
                "Some.Show.S01E01.1080p.CRAV.WEB-DL.DDP5.1.H.264-NTb",
                Some("Crave"),
            ),
            ("Crave.2012.1080p.BluRay.x264-SADPANDA", None),
            (
                "Late.Night.with.the.Devil.2023.1080p.BCORE.WEB-DL.DDP5.1-NTb",
                Some("Sony Pictures Core"),
            ),
            // `Core` inside an episode title must not match.
            (
                "Deadliest.Catch.S22E06.Rocked.to.the.Core.1080p.WEB-DL.DDP2.0.H.264-Kitsune",
                None,
            ),
            (
                "Hard Knocks 2001 S23E01 1080p MAX WEB-DL DDP2 0 x264-NTb[EZTVx.to].mkv",
                Some("HBO Max"),
            ),
            (
                "The.Rookie.S06E01.1080p.STAN.WEB-DL.DDP5.1.H.264-NTb",
                Some("Stan"),
            ),
            (
                "The.Invite.2026.2160p.PLAY.WEB-DL.DDP5.1.H.265-SCOPE",
                Some("Google TV"),
            ),
            (
                "Supergirl.2026.2160p.MA.WEB-DL.DDP5.1.Atmos.H.265-HONE",
                Some("Movies Anywhere"),
            ),
            // The audio handler consumes `DTS-HD.MA` whole, so no `MA`
            // remains for the network handler.
            (
                "Shelter.2026.2160p.UHD.BluRay.DTS-HD.MA.5.1.DV.HDR10P.x265-j3rico",
                None,
            ),
            (
                "Ghosts.of.Beirut.2023.S01.(2160p.SHO.WEB-DL.H265.SDR.DDP.5.1.English.-.HONE)",
                Some("Showtime"),
            ),
            (
                "Let.the.Right.One.In.S01.1080p.Skyshowtime.WEB-DL.AAC2.0.H.264-CHDWEB",
                Some("SkyShowtime"),
            ),
            (
                "Mission.Impossible.III.2006.2160p.SKST.WEB-DL.DD+5.1.HDR.H.265",
                Some("SkyShowtime"),
            ),
            (
                "Doom.Patrol.S01.2160p.DCU.WEB-DL.DD5.1.HDR.H.265-BTN",
                Some("DC Universe"),
            ),
            (
                "The.Ark.S02.1080p.SYFY.WEB-DL.AAC2.0.H.264-DoGSO",
                Some("SYFY"),
            ),
            (
                "The.Martian.2015.2160p.ATVP.MGMP.WEB-DL.DD.5.1.H.265-PiRaTeS",
                Some("Apple TV"),
            ),
            (
                "Jack.Reacher.Never.Go.Back.2016.2160p.MGMP.WEB-DL.DDP5.1.H.265-PiRaTeS",
                Some("MGM+"),
            ),
            (
                "Robin.Hood.2025.S01.MULTI.2160p.WEBRip.MGM+.SDR.x265.EAC3.5.1-Amen",
                Some("MGM+"),
            ),
            (
                "The Vet Life S02E01 Dunk-A-Doctor 1080p ANPL WEB-DL AAC2 0 H 264-RTN",
                Some("Animal Planet"),
            ),
            // Ambiguous tags must never eat a title word.
            ("Mad Max Fury Road", None),
            (
                "Mad.Max.Fury.Road.2015.1080p.BluRay.DDP5.1.x265.10bit-GalaxyRG265[TGx]",
                None,
            ),
            ("Max.Payne.2008.1080p.BluRay.x264-MEDiAxSHOCK", None),
            ("Big.Stan.2007.1080p.BluRay.Remux.DTS-HD.HR.5.1", None),
            (
                "Stan.Against.Evil.S01E01.1080p.WEB-DL.DD5.1.H.264-NTb",
                None,
            ),
            ("Ma.2019.1080p.BluRay.REMUX.AVC.DTS-HD.MA.5.1-EPSiLON", None),
            // An episode title starting with a tag word.
            ("Suits - S01E07 - Play the Man - Bluray-720p", None),
            // No network at all.
            ("Nocturnal Animals 2016 VFF 1080p BluRay DTS HEVC-HD2", None),
            ("Gotham S03E17 XviD-AFG", None),
            ("Jimmy Kimmel 2017 05 03 720p HDTV DD5 1 MPEG2-CTL", None),
            (
                "[Anime Time] Re Zero kara Hajimeru Isekai Seikatsu (Season 2 Part 1) [1080p][HEVC10bit x265][Multi Sub]",
                None,
            ),
            (
                "[naiyas] Fate Stay Night - Unlimited Blade Works Movie [BD 1080P HEVC10 QAACx2 Dual Audio]",
                None,
            ),
        ];
        for &(title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.network.as_deref(), expected, "title: {title}");
        }
    }
}
