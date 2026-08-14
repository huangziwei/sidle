//! The session parser against verbatim lines from each reader stack.
//!
//! The unit tests build lines from a template, which proves the rules but not
//! that the template matches what a Kindle writes. These lines are copied
//! unedited out of a device's own log, so they fail if a shape assumption is
//! wrong. The device redacts the book itself — `Title:<private>,Asin:<private>`
//! — so a line carries no more than positions and counters.

use sidle_core::library::reading_log::parse_sessions;

/// The Corretto/KPP reader. Its `SyslogFormatter` drops the head of a payload,
/// so the open is the only line here that begins with its own event name: the
/// second names none at all, and the close arrives after the tail of the
/// payload before it.
const CORRETTO: &[&str] = &[
    "260811:072945.607 java[8437]: I ReadingTimerController:Information::OpenBook,CurrentVersionUsed:0,StoredBookData:null,Title:<private>,Asin:<private>;",
    "260811:072948.844 java[8437]: I ReadingTimerController:Information::LogDataReturnCode:0,GlobalWPM:233.44044363899735,GlobalTime:0,GlobalWords:6139,BookEndPosition.FromBook:YJPosition: AZI/AAAAAAAA:938018,BookEndPosition.LastWordPos.override:YJPosition: Aag/AACDAQAA:938016,CurrentPos:YJPosition: AWUDAAAAAAAA:2,EndPos:YJPosition: Aag/AACDAQAA:938016,PosLeft:938014,%Left:0.9998635557374812,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,NextTOCEntryPosition:YJPosition: AaUDAAAAAAAA:1454,NextTOCEntryLength:8,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: AWUDAAAAAAAA:2,EndPos:YJPosition: AaUDAAAAAAAA:1454,PosLeft:1452,%Left:0.0015008868877063718,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,TimeLeftInBookString:Learning reading speed...,TimeLeftInSectionString:Learning reading speed...,GlobalWPM:233.44044363899735,GlobalTime:0,GlobalWords:6139;",
    "260811:074139.882 java[8437]: I ReadingTimerController:Information::DataSufficient:YES,FinalWPM:290.0472366277654,NewTimeLeft:1440,OldTimeLeft:1521,TimeLeftInBookString:8 hrs 44 mins left in book,TimeLeftInSectionString:24 mins left in chapter;CloseBook,Title:<private>,Asin:<private>,PageStartPos:YJPosition: AR4GAAAAAAAA:39799,IntervalTime:68346,IntervalWords:551,IntervalWPM:483.7152137652533,ScreenStart:YJPosition: AR4GAAAAAAAA:39799,ScreenEnd:YJPosition: AUoGAABCAAAA:43003,SkipAvgReason:RTC_Close,Interval%:0.0034111065629690226,TotalTime:462142,TotalWords:2148,TotalWPM:290.0472366277654,Total%:0.013507981989357338,LogDataReturnCode:551,GlobalWPM:233.44044363899735,GlobalTime:462142,GlobalWords:6139,CurrentPos:YJPosition: AR4GAAAAAAAA:39799,EndPos:YJPosition: Aag/AACDAQAA:938016,PosLeft:898217,%Left:0.9575658343566653,CurrentPagePosDiff:3204,TimeForPage:141.62070412753948,PosPassed:false,DataSufficient:YES,FinalWPM:290.0472366277654,NewTimeLeft:31440,OldTimeLeft:32760,NextTOCEntryPosition:YJPosition: AaUIAAAAAAAA:81515,NextTOCEntryLength:47,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: AR4GAAAAAAAA:39799,EndPos:YJPosition: AaUIAAAAAAAA:81515,PosLeft:41716,%Left:0.04448082958111611,CurrentPagePosDiff:3204,TimeForPage:141.62070412753948,PosPassed:false,DataSufficient:YES,FinalWPM:290.0472366277654,NewTimeLeft:1440,OldTimeLeft:1521,TimeLeftInBookString:8 hrs 44 mins left in book,TimeLeftInSectionString:24 mins left in chapter;",
];

/// The `cvm` reader, which names every event and puts the name first.
const CVM: &[&str] = &[
    "260807:101501 cvm[6144]: I ReadingTimerController:Information::NextPage,Verdict:Processed,PageStartPos:YJPosition: AfQJAAAAAAAA:54205,IntervalTime:39890,IntervalWords:320,IntervalWPM:481.3236400100276,ScreenStart:YJPosition: AeoJAAAiAAAA:53882,ScreenEnd:YJPosition: AfMJAABEAAAA:54204,BookInfoKnownWords:68074,BookInfoKnown%:0.42245072836332476,Interval%:0.0019280205655526905,TotalTime:7390020,TotalWords:49583,TotalWPM:427.95245384175865,Total%:0.30248500428449004,LogDataReturnCode:320,GlobalWPM:336.16893495435625,GlobalTime:7390020,GlobalWords:1544302,CurrentPos:YJPosition: AfQJAAAAAAAA:54205,EndPos:YJPosition: AbcVAAAPAAAA:148207,PosLeft:94002,%Left:0.6450299914310198,CurrentPagePosDiff:301,TimeForPage:52.47361717820563,PosPassed:false,DataSufficient:YES,FinalWPM:427.95245384175865,NewTimeLeft:14820,OldTimeLeft:15758,NextTOCEntryPosition:YJPosition: AT4KAAAAAAAA:56499,NextTOCEntryLength:10,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: AfQJAAAAAAAA:54205,EndPos:YJPosition: AT4KAAAAAAAA:56499,PosLeft:2294,%Left:0.01585261353898887,CurrentPagePosDiff:301,TimeForPage:52.47361717820563,PosPassed:false,DataSufficient:YES,FinalWPM:427.95245384175865,NewTimeLeft:360,OldTimeLeft:387,TimeLeftInBookString:4 hrs 7 mins left in book,TimeLeftInSectionString:6 mins left in chapter;",
    "260807:101543 cvm[6144]: I ReadingTimerController:Information::NextPage,Verdict:Processed,PageStartPos:YJPosition: Af8JAAAAAAAA:54507,IntervalTime:41443,IntervalWords:294,IntervalWPM:425.6448616171609,ScreenStart:YJPosition: AfQJAAAAAAAA:54205,ScreenEnd:YJPosition: Af4JAABQAAAA:54506,BookInfoKnownWords:68368,BookInfoKnown%:0.4245929734361611,Interval%:0.002142245072836335,TotalTime:7431463,TotalWords:49877,TotalWPM:427.93413961775394,Total%:0.3046272493573264,LogDataReturnCode:294,GlobalWPM:336.1875447560748,GlobalTime:7431463,GlobalWords:1544596,CurrentPos:YJPosition: Af8JAAAAAAAA:54507,EndPos:YJPosition: AbcVAAAPAAAA:148207,PosLeft:93700,%Left:0.6426735218508998,CurrentPagePosDiff:322,TimeForPage:56.39709232463289,PosPassed:false,DataSufficient:YES,FinalWPM:427.93413961775394,NewTimeLeft:14700,OldTimeLeft:15678,NextTOCEntryPosition:YJPosition: AT4KAAAAAAAA:56499,NextTOCEntryLength:10,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: Af8JAAAAAAAA:54507,EndPos:YJPosition: AT4KAAAAAAAA:56499,PosLeft:1992,%Left:0.01349614395886889,CurrentPagePosDiff:322,TimeForPage:56.39709232463289,PosPassed:false,DataSufficient:YES,FinalWPM:427.93413961775394,NewTimeLeft:300,OldTimeLeft:329,TimeLeftInBookString:4 hrs 5 mins left in book,TimeLeftInSectionString:5 mins left in chapter;",
];

#[test]
fn a_session_survives_a_payload_losing_its_event_name() {
    let out = parse_sessions(CORRETTO.iter().copied(), None);
    assert_eq!(out.len(), 1);
    // The counter reads 462142 ms at the close and the book opened from zero,
    // so the sitting is 462 s. Keyed on the event name instead, none of these
    // three lines is a page event and the close is not first on its line, so
    // there is no session at all.
    assert_eq!(out[0].seconds, 462);
    assert_eq!(out[0].end_position, 938_016);
    // The open is where the sitting started, even though the line carrying it
    // is not itself an observation, and even though the observation that opens
    // the run states a position and no counter: the floor arrives with the
    // close, twelve minutes on, and is adopted by the run already under way.
    assert_eq!(out[0].started_at, "2026-08-11T07:29:45");
    assert_eq!(out[0].ended_at, "2026-08-11T07:41:39");
    // Nothing named a turn, and none is invented.
    assert_eq!(out[0].page_turns, 0);
}

/// One page turn from the same reader, cut so the line opens with the *tail* of
/// the payload before it: a chapter block (`EndPos` 8548, the value this
/// payload's own `NextTOCEntryPosition` also states) standing in front of
/// everything this event is about. The book's own block — `EndPos` 327525 — is
/// where it belongs, ahead of the `NextTOCEntry…` group.
const CORRETTO_CUT_PAGE: &str = "260809:124414.643 java[5602]: I ReadingTimerController:Information::CurrentPos:YJPosition: AawEAAAdAAAA:4327,EndPos:YJPosition: AXYDAAAAAAAA:8548,PosLeft:4221,%Left:0.013015826061233544,CurrentPagePosDiff:817,TimeForPage:114.59553412578684,PosPassed:false,DataSufficient:NO,TimeLeftInBookString:Learning reading speed...,TimeLeftInSectionString:Learning reading speed...,PageStartPos:YJPosition: AbAEAAAAAAAA:5145,IntervalTime:6210,IntervalWords:389,IntervalWPM:3758.454106280193,ScreenStart:YJPosition: AbAEAAAAAAAA:5145,ScreenEnd:YJPosition: AbQEAAA6AAAA:5894,SkipAvgReason:Sample out of range,Interval%:-1.0,TotalTime:0,TotalWords:0,TotalWPM:0.0,Total%:0.0,SkipAvgReason:Sample out of range,LogDataReturnCode:389,GlobalWPM:230.89905031513652,GlobalTime:0,GlobalWords:6079,CurrentPos:YJPosition: AbAEAAAAAAAA:5145,EndPos:YJPosition: AfgWAAAkAAAA:327525,PosLeft:322380,%Left:0.981511610708475,CurrentPagePosDiff:749,TimeForPage:101.0831355440614,PosPassed:false,DataSufficient:NO,NextTOCEntryPosition:YJPosition: AXYDAAAAAAAA:8548,NextTOCEntryLength:12,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: AbAEAAAAAAAA:5145,EndPos:YJPosition: AXYDAAAAAAAA:8548,PosLeft:3403,%Left:0.010501405117586154,CurrentPagePosDiff:749,TimeForPage:101.0831355440614,PosPassed:false,DataSufficient:NO,TimeLeftInBookString:Learning reading speed...,TimeLeftInSectionString:Learning reading speed...;Reading_Interrupted,Reason:1;";

/// A payload whose head was cut must still be filed under its book, not under
/// the chapter whose block the cut left in front of it.
///
/// A chapter end moves as the reader advances, so reading it as the book's
/// identity restarts the sitting at every chapter — and a run of one
/// observation spans no counter, so each fragment is dropped as zero-length and
/// a morning's reading stores nothing at all.
#[test]
fn a_cut_payload_is_filed_under_its_book_not_the_chapter_in_front_of_it() {
    let out = parse_sessions([CORRETTO_CUT_PAGE], None);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].end_position, 327_525,
        "8548 is the chapter's end — it is also this payload's NextTOCEntryPosition"
    );
}

/// A sitting in a book the device never times, as the Scribe logged one: the
/// book is opened, states its positions, and is reopened later still declaring
/// `TimeRead:0 sec.` — the reader's own counter has not moved because it counts
/// words and this book has none (`Num words known in book:0`).
///
/// Its length is therefore not in the log at all, and the only thing that
/// bounds it is the device's power state.
const WORDLESS: &[&str] = &[
    "260814:112035 java[9795]: I ReadingTimerController:Information::OpenBook,CurrentVersionUsed:0,StoredBookData:null,Title:<private>,Asin:<private>;",
    "260814:112414 java[9795]: I ReadingTimerController:Information::BookInfo:BookInfo:Num words known in book:0:Percentage of book for the known words:0.0:,GlobalWords:12080,CurrentPos:YJPosition: AWAEAAAAAAAA:57,EndPos:YJPosition: ARsHAAAAAAAA:442,PosLeft:385,%Left:0.859375,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,NextTOCEntryPosition:YJPosition: AbUDAAAAAAAA:158,NextTOCEntryLength:64,CurrentPos:YJPosition: AWAEAAAAAAAA:57,EndPos:YJPosition: AbUDAAAAAAAA:158;",
    "260814:113238 java[9795]: I ReadingTimerController:Information::OpenBook,CurrentVersionUsed:0,StoredBookData:TimeRead:0 sec. WPM:0. Version:0,Title:<private>,Asin:<private>;",
    "260814:113240 java[9795]: I ReadingTimerController:Information::BookInfo:BookInfo:Num words known in book:0:Percentage of book for the known words:0.0:,GlobalWords:12080,CurrentPos:YJPosition: AQcFAAAAAAAA:145,EndPos:YJPosition: ARsHAAAAAAAA:442,PosLeft:297,%Left:0.671875,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,NextTOCEntryPosition:YJPosition: AbUDAAAAAAAA:158,NextTOCEntryLength:64,CurrentPos:YJPosition: AQcFAAAAAAAA:145,EndPos:YJPosition: AbUDAAAAAAAA:158;",
];

/// The device's own power records, verbatim in shape: awake from 11:19:00,
/// asleep from 11:35:00.
const POWER: &[&str] = &[
    "260814:111900.726 fastmetrics[9842]: D fastmetrics:KindleFastMetricsPublisher:[24109.944489]: Emitting a new record. SchemaName[ereader_powerd_state_change], Fields[{ \t\"curr_state\" : \"ACTIVE\", \t\"prev_state\" : \"SCREEN SAVER\" } ]. :",
    "260814:113500.549 fastmetrics[9842]: D fastmetrics:KindleFastMetricsPublisher:[26548.733985]: Emitting a new record. SchemaName[ereader_powerd_state_change], Fields[{ \t\"curr_state\" : \"SCREEN SAVER\", \t\"prev_state\" : \"ACTIVE\" } ]. :",
];

/// A book the device refuses to time is measured by how long the device was
/// awake with it open, and says so.
#[test]
fn a_book_the_device_never_times_is_measured_by_its_awake_time() {
    let mut all: Vec<&str> = WORDLESS.iter().chain(POWER).copied().collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    let s = &out[0];
    assert!(
        s.estimated,
        "no counter moved, so this cannot be counted time"
    );
    // The sitting runs from the open to where the reader was last seen —
    // 11:20:35 to 11:32:40 — and the device was ACTIVE across all of it.
    assert_eq!(s.seconds, 12 * 60 + 5);
    assert_eq!(s.end_position, 442, "the book, not its chapter at 158");
    // Its hours add back up to it, so the clock and the calendar agree.
    assert_eq!(s.hours.iter().map(|(_, sec)| sec).sum::<i64>(), s.seconds);
}

/// Without power records there is no bound, and an unbounded wall clock would
/// credit a book left open overnight with the night. The device's own answer
/// for such a book is zero, and zero is what it stays.
#[test]
fn a_book_the_device_never_times_is_not_guessed_at_without_a_bound() {
    let out = parse_sessions(WORDLESS.iter().copied(), None);
    assert!(out.iter().all(|s| s.seconds == 0));
}

/// The bound never displaces a figure the device did count.
#[test]
fn a_counted_sitting_is_untouched_by_the_power_records() {
    let mut all: Vec<&str> = CORRETTO.iter().chain(POWER).copied().collect();
    all.sort();
    let out = parse_sessions(all, None);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].seconds, 462, "the counter, not the awake time");
    assert!(!out[0].estimated);
}

#[test]
fn a_named_page_event_is_read_exactly_as_before() {
    let out = parse_sessions(CVM.iter().copied(), None);
    assert_eq!(out.len(), 1);
    // 7431463 - 7390020, to the second.
    assert_eq!(out[0].seconds, 41);
    assert_eq!(out[0].words, 294);
    // The book's end position, not the chapter's 56499.
    assert_eq!(out[0].end_position, 148_207);
    assert_eq!(out[0].page_turns, 2);
}
