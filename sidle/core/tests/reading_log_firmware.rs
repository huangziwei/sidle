//! `parse_sessions` over lines copied unedited from a device's own log.
//! Every `ReadingTimerController` line reads `Title:<private>,Asin:<private>`
//! and carries positions and counters.

use sidle_core::library::reading_log::{Measure, parse_sessions};

/// `java[8437]` lines with the head of each payload cut. `OpenBook` heads the
/// first, the second names no event, and `CloseBook` follows the tail of the
/// payload before it on the third.
const CORRETTO: &[&str] = &[
    "260811:072945.607 java[8437]: I ReadingTimerController:Information::OpenBook,CurrentVersionUsed:0,StoredBookData:null,Title:<private>,Asin:<private>;",
    "260811:072948.844 java[8437]: I ReadingTimerController:Information::LogDataReturnCode:0,GlobalWPM:233.44044363899735,GlobalTime:0,GlobalWords:6139,BookEndPosition.FromBook:YJPosition: AZI/AAAAAAAA:938018,BookEndPosition.LastWordPos.override:YJPosition: Aag/AACDAQAA:938016,CurrentPos:YJPosition: AWUDAAAAAAAA:2,EndPos:YJPosition: Aag/AACDAQAA:938016,PosLeft:938014,%Left:0.9998635557374812,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,NextTOCEntryPosition:YJPosition: AaUDAAAAAAAA:1454,NextTOCEntryLength:8,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: AWUDAAAAAAAA:2,EndPos:YJPosition: AaUDAAAAAAAA:1454,PosLeft:1452,%Left:0.0015008868877063718,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,TimeLeftInBookString:Learning reading speed...,TimeLeftInSectionString:Learning reading speed...,GlobalWPM:233.44044363899735,GlobalTime:0,GlobalWords:6139;",
    "260811:074139.882 java[8437]: I ReadingTimerController:Information::DataSufficient:YES,FinalWPM:290.0472366277654,NewTimeLeft:1440,OldTimeLeft:1521,TimeLeftInBookString:8 hrs 44 mins left in book,TimeLeftInSectionString:24 mins left in chapter;CloseBook,Title:<private>,Asin:<private>,PageStartPos:YJPosition: AR4GAAAAAAAA:39799,IntervalTime:68346,IntervalWords:551,IntervalWPM:483.7152137652533,ScreenStart:YJPosition: AR4GAAAAAAAA:39799,ScreenEnd:YJPosition: AUoGAABCAAAA:43003,SkipAvgReason:RTC_Close,Interval%:0.0034111065629690226,TotalTime:462142,TotalWords:2148,TotalWPM:290.0472366277654,Total%:0.013507981989357338,LogDataReturnCode:551,GlobalWPM:233.44044363899735,GlobalTime:462142,GlobalWords:6139,CurrentPos:YJPosition: AR4GAAAAAAAA:39799,EndPos:YJPosition: Aag/AACDAQAA:938016,PosLeft:898217,%Left:0.9575658343566653,CurrentPagePosDiff:3204,TimeForPage:141.62070412753948,PosPassed:false,DataSufficient:YES,FinalWPM:290.0472366277654,NewTimeLeft:31440,OldTimeLeft:32760,NextTOCEntryPosition:YJPosition: AaUIAAAAAAAA:81515,NextTOCEntryLength:47,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: AR4GAAAAAAAA:39799,EndPos:YJPosition: AaUIAAAAAAAA:81515,PosLeft:41716,%Left:0.04448082958111611,CurrentPagePosDiff:3204,TimeForPage:141.62070412753948,PosPassed:false,DataSufficient:YES,FinalWPM:290.0472366277654,NewTimeLeft:1440,OldTimeLeft:1521,TimeLeftInBookString:8 hrs 44 mins left in book,TimeLeftInSectionString:24 mins left in chapter;",
];

/// `cvm[6144]` lines, each naming its event — `NextPage` — first.
const CVM: &[&str] = &[
    "260807:101501 cvm[6144]: I ReadingTimerController:Information::NextPage,Verdict:Processed,PageStartPos:YJPosition: AfQJAAAAAAAA:54205,IntervalTime:39890,IntervalWords:320,IntervalWPM:481.3236400100276,ScreenStart:YJPosition: AeoJAAAiAAAA:53882,ScreenEnd:YJPosition: AfMJAABEAAAA:54204,BookInfoKnownWords:68074,BookInfoKnown%:0.42245072836332476,Interval%:0.0019280205655526905,TotalTime:7390020,TotalWords:49583,TotalWPM:427.95245384175865,Total%:0.30248500428449004,LogDataReturnCode:320,GlobalWPM:336.16893495435625,GlobalTime:7390020,GlobalWords:1544302,CurrentPos:YJPosition: AfQJAAAAAAAA:54205,EndPos:YJPosition: AbcVAAAPAAAA:148207,PosLeft:94002,%Left:0.6450299914310198,CurrentPagePosDiff:301,TimeForPage:52.47361717820563,PosPassed:false,DataSufficient:YES,FinalWPM:427.95245384175865,NewTimeLeft:14820,OldTimeLeft:15758,NextTOCEntryPosition:YJPosition: AT4KAAAAAAAA:56499,NextTOCEntryLength:10,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: AfQJAAAAAAAA:54205,EndPos:YJPosition: AT4KAAAAAAAA:56499,PosLeft:2294,%Left:0.01585261353898887,CurrentPagePosDiff:301,TimeForPage:52.47361717820563,PosPassed:false,DataSufficient:YES,FinalWPM:427.95245384175865,NewTimeLeft:360,OldTimeLeft:387,TimeLeftInBookString:4 hrs 7 mins left in book,TimeLeftInSectionString:6 mins left in chapter;",
    "260807:101543 cvm[6144]: I ReadingTimerController:Information::NextPage,Verdict:Processed,PageStartPos:YJPosition: Af8JAAAAAAAA:54507,IntervalTime:41443,IntervalWords:294,IntervalWPM:425.6448616171609,ScreenStart:YJPosition: AfQJAAAAAAAA:54205,ScreenEnd:YJPosition: Af4JAABQAAAA:54506,BookInfoKnownWords:68368,BookInfoKnown%:0.4245929734361611,Interval%:0.002142245072836335,TotalTime:7431463,TotalWords:49877,TotalWPM:427.93413961775394,Total%:0.3046272493573264,LogDataReturnCode:294,GlobalWPM:336.1875447560748,GlobalTime:7431463,GlobalWords:1544596,CurrentPos:YJPosition: Af8JAAAAAAAA:54507,EndPos:YJPosition: AbcVAAAPAAAA:148207,PosLeft:93700,%Left:0.6426735218508998,CurrentPagePosDiff:322,TimeForPage:56.39709232463289,PosPassed:false,DataSufficient:YES,FinalWPM:427.93413961775394,NewTimeLeft:14700,OldTimeLeft:15678,NextTOCEntryPosition:YJPosition: AT4KAAAAAAAA:56499,NextTOCEntryLength:10,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: Af8JAAAAAAAA:54507,EndPos:YJPosition: AT4KAAAAAAAA:56499,PosLeft:1992,%Left:0.01349614395886889,CurrentPagePosDiff:322,TimeForPage:56.39709232463289,PosPassed:false,DataSufficient:YES,FinalWPM:427.93413961775394,NewTimeLeft:300,OldTimeLeft:329,TimeLeftInBookString:4 hrs 5 mins left in book,TimeLeftInSectionString:5 mins left in chapter;",
];

#[test]
fn a_session_survives_a_payload_losing_its_event_name() {
    let out = parse_sessions(CORRETTO.iter().copied(), None);
    assert_eq!(out.len(), 1);
    // TotalTime 462142 at CloseBook against StoredBookData:null at OpenBook.
    assert_eq!(out[0].seconds, 462);
    assert_eq!(out[0].end_position, 938_016);
    // The OpenBook line's stamp, and the CloseBook line's.
    assert_eq!(out[0].started_at, "2026-08-11T07:29:45");
    assert_eq!(out[0].ended_at, "2026-08-11T07:41:39");
    // No line names NextPage.
    assert_eq!(out[0].page_turns, 0);
}

/// `CORRETTO_CUT_PAGE` opens with the tail of the payload before it: `EndPos`
/// 8548, the position its own `NextTOCEntryPosition` names. `EndPos` 327525
/// sits ahead of the `NextTOCEntryPosition` group.
const CORRETTO_CUT_PAGE: &str = "260809:124414.643 java[5602]: I ReadingTimerController:Information::CurrentPos:YJPosition: AawEAAAdAAAA:4327,EndPos:YJPosition: AXYDAAAAAAAA:8548,PosLeft:4221,%Left:0.013015826061233544,CurrentPagePosDiff:817,TimeForPage:114.59553412578684,PosPassed:false,DataSufficient:NO,TimeLeftInBookString:Learning reading speed...,TimeLeftInSectionString:Learning reading speed...,PageStartPos:YJPosition: AbAEAAAAAAAA:5145,IntervalTime:6210,IntervalWords:389,IntervalWPM:3758.454106280193,ScreenStart:YJPosition: AbAEAAAAAAAA:5145,ScreenEnd:YJPosition: AbQEAAA6AAAA:5894,SkipAvgReason:Sample out of range,Interval%:-1.0,TotalTime:0,TotalWords:0,TotalWPM:0.0,Total%:0.0,SkipAvgReason:Sample out of range,LogDataReturnCode:389,GlobalWPM:230.89905031513652,GlobalTime:0,GlobalWords:6079,CurrentPos:YJPosition: AbAEAAAAAAAA:5145,EndPos:YJPosition: AfgWAAAkAAAA:327525,PosLeft:322380,%Left:0.981511610708475,CurrentPagePosDiff:749,TimeForPage:101.0831355440614,PosPassed:false,DataSufficient:NO,NextTOCEntryPosition:YJPosition: AXYDAAAAAAAA:8548,NextTOCEntryLength:12,NextTOCEntryLevel:0,NextTOCEntryType:null,CurrentPos:YJPosition: AbAEAAAAAAAA:5145,EndPos:YJPosition: AXYDAAAAAAAA:8548,PosLeft:3403,%Left:0.010501405117586154,CurrentPagePosDiff:749,TimeForPage:101.0831355440614,PosPassed:false,DataSufficient:NO,TimeLeftInBookString:Learning reading speed...,TimeLeftInSectionString:Learning reading speed...;Reading_Interrupted,Reason:1;";

/// `end_position` takes `EndPos` 327525 and not `EndPos` 8548.
#[test]
fn a_cut_payload_is_filed_under_its_book_not_the_chapter_in_front_of_it() {
    let out = parse_sessions([CORRETTO_CUT_PAGE], None);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].end_position, 327_525,
        "8548 is the chapter's end — it is also this payload's NextTOCEntryPosition"
    );
}

/// Two `OpenBook` lines around a book whose `Num words known in book` is 0.
/// The second `OpenBook` states `TimeRead:0 sec.`, and no line carries
/// `TotalTime`.
const WORDLESS: &[&str] = &[
    "260814:112035 java[9795]: I ReadingTimerController:Information::OpenBook,CurrentVersionUsed:0,StoredBookData:null,Title:<private>,Asin:<private>;",
    "260814:112414 java[9795]: I ReadingTimerController:Information::BookInfo:BookInfo:Num words known in book:0:Percentage of book for the known words:0.0:,GlobalWords:12080,CurrentPos:YJPosition: AWAEAAAAAAAA:57,EndPos:YJPosition: ARsHAAAAAAAA:442,PosLeft:385,%Left:0.859375,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,NextTOCEntryPosition:YJPosition: AbUDAAAAAAAA:158,NextTOCEntryLength:64,CurrentPos:YJPosition: AWAEAAAAAAAA:57,EndPos:YJPosition: AbUDAAAAAAAA:158;",
    "260814:113238 java[9795]: I ReadingTimerController:Information::OpenBook,CurrentVersionUsed:0,StoredBookData:TimeRead:0 sec. WPM:0. Version:0,Title:<private>,Asin:<private>;",
    "260814:113240 java[9795]: I ReadingTimerController:Information::BookInfo:BookInfo:Num words known in book:0:Percentage of book for the known words:0.0:,GlobalWords:12080,CurrentPos:YJPosition: AQcFAAAAAAAA:145,EndPos:YJPosition: ARsHAAAAAAAA:442,PosLeft:297,%Left:0.671875,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,NextTOCEntryPosition:YJPosition: AbUDAAAAAAAA:158,NextTOCEntryLength:64,CurrentPos:YJPosition: AQcFAAAAAAAA:145,EndPos:YJPosition: AbUDAAAAAAAA:158;",
];

/// `ereader_powerd_state_change` records: `ACTIVE` at 11:19:00, `SCREEN SAVER`
/// at 11:35:00.
const POWER: &[&str] = &[
    "260814:111900.726 fastmetrics[9842]: D fastmetrics:KindleFastMetricsPublisher:[24109.944489]: Emitting a new record. SchemaName[ereader_powerd_state_change], Fields[{ \t\"curr_state\" : \"ACTIVE\", \t\"prev_state\" : \"SCREEN SAVER\" } ]. :",
    "260814:113500.549 fastmetrics[9842]: D fastmetrics:KindleFastMetricsPublisher:[26548.733985]: Emitting a new record. SchemaName[ereader_powerd_state_change], Fields[{ \t\"curr_state\" : \"SCREEN SAVER\", \t\"prev_state\" : \"ACTIVE\" } ]. :",
];

/// The same two transitions as `powerd` LIPC events: `outOfScreenSaver` at
/// 11:19:00, `goingToScreenSaver` at 11:35:00.
const POWER_EVENTS: &[&str] = &[
    "260814:111900 powerd[4213]: I lipc:evts:name=outOfScreenSaver, origin=com.lab126.powerd, fparam=2:Event sent",
    "260814:113500 powerd[4213]: I lipc:evts:name=goingToScreenSaver, origin=com.lab126.powerd, fparam=3:Event sent",
];

/// `wakeupFromSuspend` and `resuming` at 11:25:00, `suspending` at 11:28:30,
/// with no `outOfScreenSaver` between them.
const POWER_UNATTENDED: &[&str] = &[
    "260814:112500 powerd[4213]: I lipc:evts:name=wakeupFromSuspend, origin=com.lab126.powerd, fparam=154:Event sent",
    "260814:112500 powerd[4213]: I lipc:evts:name=resuming, origin=com.lab126.powerd:Event sent",
    "260814:112830 powerd[4213]: I lipc:evts:name=suspending, origin=com.lab126.powerd, fparam=mem:Event sent",
];

/// `WORDLESS` with `POWER` gives `Measure::Awake` and its `seconds`.
#[test]
fn a_book_the_device_never_times_is_measured_by_its_awake_time() {
    let mut all: Vec<&str> = WORDLESS.iter().chain(POWER).copied().collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    let s = &out[0];
    assert_eq!(s.measure, Measure::Awake, "no TotalTime moved");
    // 11:20:35 to 11:32:40, inside the ACTIVE span.
    assert_eq!(s.seconds, 12 * 60 + 5);
    assert_eq!(s.end_position, 442, "the book, not its chapter at 158");
    assert_eq!(s.hours.iter().map(|(_, sec)| sec).sum::<i64>(), s.seconds);
}

/// `POWER_EVENTS` alone gives the `seconds` `POWER` gives.
#[test]
fn the_bound_is_read_from_the_lipc_events_where_the_record_is_not_written() {
    let mut all: Vec<&str> = WORDLESS.iter().chain(POWER_EVENTS).copied().collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].measure, Measure::Awake);
    assert_eq!(out[0].seconds, 12 * 60 + 5);
}

/// A page record, verbatim in shape, at `stamp` carrying `words` on the page.
fn page(stamp: &str, words: i64) -> String {
    format!(
        r#"{stamp} fastmetrics[9842]: D fastmetrics:KindleFastMetricsPublisher:[1.0]: Emitting a new record. SchemaName[ereader_book_consume_content], Fields[{{ 	"context" : "Book:Reading:MainContent", 	"end_position" : 4133, 	"span_type" : "Text", 	"start_position" : 3227, 	"words_count" : {words} }} ]. :"#
    )
}

/// A forward turn, in the shape the `cvm` stack writes it.
fn turn(stamp: &str) -> String {
    format!(
        r#"{stamp} fastmetrics[9842]: D fastmetrics:KindleFastMetricsPublisher:[1.0]: Emitting a new record. SchemaName[ereader_book_linear_page_actions], Fields[{{ 	"action_id" : "NextPageWithSwipe", 	"context" : "Book:Reading:MainContent" }} ]. :"#
    )
}

/// A book `ReadingTimerController` never times is measured page by page, not
/// bounded by the awake span.
///
/// Four pages at 11:21, 11:22, 11:24 and 11:26:30. The book states no words, so
/// each dwell falls on the wordless branch, [3 s, 120 s]: 60 s, then 120 s and
/// 120 s clamped down from 120 s and 150 s. The page open at the end closes
/// nothing.
#[test]
fn a_book_the_device_never_times_is_measured_by_its_page_dwell() {
    let pages = [
        page("260814:112100", 0),
        page("260814:112200", 0),
        page("260814:112400", 0),
        page("260814:112630", 0),
    ];
    let mut all: Vec<&str> = WORDLESS
        .iter()
        .chain(POWER)
        .copied()
        .chain(pages.iter().map(String::as_str))
        .collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].measure, Measure::Dwell);
    assert_eq!(out[0].seconds, 60 + 120 + 120);
    assert_eq!(out[0].end_position, 442);
    assert_eq!(
        out[0].hours.iter().map(|(_, s)| s).sum::<i64>(),
        out[0].seconds
    );
}

/// A page turned before the first `ReadingTimerController` line that carries a
/// position still counts.
///
/// On the stack that refuses to time such a book, that line can lag the open by
/// minutes — here the open is 11:20:35 and the first positioned line 11:24:14.
#[test]
fn a_page_turned_before_the_run_opens_is_not_lost() {
    let early = [page("260814:112100", 0), page("260814:112200", 0)];
    let mut all: Vec<&str> = WORDLESS
        .iter()
        .chain(POWER)
        .copied()
        .chain(early.iter().map(String::as_str))
        .collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].measure, Measure::Dwell);
    assert_eq!(out[0].seconds, 60, "11:21:00 to 11:22:00");
}

/// The dwell never displaces a figure the device counted.
#[test]
fn a_counted_sitting_is_untouched_by_the_page_records() {
    let pages = [page("260811:073000", 300), page("260811:073500", 300)];
    let mut all: Vec<&str> = CORRETTO
        .iter()
        .copied()
        .chain(pages.iter().map(String::as_str))
        .collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].measure, Measure::Counted);
    assert_eq!(out[0].seconds, 462);
}

/// Turns come from one stack or the other, never both.
///
/// `CVM` names two `NextPage` events and writes page records for the same
/// turns; summing them reports four.
#[test]
fn a_turn_named_by_both_stacks_is_counted_once() {
    let turns = [turn("260807:101501"), turn("260807:101543")];
    let mut all: Vec<&str> = CVM
        .iter()
        .copied()
        .chain(turns.iter().map(String::as_str))
        .collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].page_turns, 2);
}

/// A stack that names no turn takes them from the page records.
#[test]
fn turns_are_taken_from_the_page_records_where_no_event_names_one() {
    let turns = [turn("260814:112500"), turn("260814:112600")];
    let mut all: Vec<&str> = WORDLESS
        .iter()
        .chain(POWER)
        .copied()
        .chain(turns.iter().map(String::as_str))
        .collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].page_turns, 2);
}

/// The zone comes off a record stating an instant the prefix also states.
///
/// `close_timestamp` 1786882311101 is 12:11:51 UTC, under the prefix
/// `260816:141151` — +02:00, and the reading-timer lines state no instant at
/// all.
#[test]
fn a_sitting_records_the_zone_its_clock_was_in() {
    let close = r#"260816:141151.131 fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[6587.267540]: Emitting a new record. SchemaName[ereader_close_book], Fields[{ 	"close_method" : "Navigation", 	"close_position" : 5070, 	"close_timestamp" : 1786882311101, 	"is_opened_by_kpp_reader" : "Yes" } ]. :"#;
    let arc = [
        "260816:141126 java[9795]: I ReadingTimerController:Information::OpenBook,CurrentVersionUsed:0,StoredBookData:null,Title:<private>,Asin:<private>;".to_string(),
        "260816:141130 java[9795]: I ReadingTimerController:Information::BookInfo:BookInfo:Num words known in book:0:Percentage of book for the known words:0.0:,CurrentPos:YJPosition: AWAEAAAAAAAA:184,EndPos:YJPosition: ARsHAAAAAAAA:23794,PosLeft:1,%Left:0.1,CurrentPagePosDiff:0,TimeForPage:-4.0,PosPassed:false,DataSufficient:YES,NextTOCEntryPosition:YJPosition: AbUDAAAAAAAA:190,NextTOCEntryLength:64,CurrentPos:YJPosition: AWAEAAAAAAAA:184,EndPos:YJPosition: AbUDAAAAAAAA:190;".to_string(),
        close.to_string(),
    ];
    let mut all: Vec<&str> = arc.iter().map(String::as_str).collect();
    all.sort();
    let out = parse_sessions(all, None);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].tz_offset_s, Some(2 * 3600));
    assert_eq!(
        out[0].started_at, "2026-08-16T14:11:26",
        "the stored stamps stay local"
    );
}

/// `POWER` and `POWER_EVENTS` together parse as `POWER` alone.
#[test]
fn a_device_writing_both_power_families_is_measured_once() {
    let mut both: Vec<&str> = WORDLESS
        .iter()
        .chain(POWER)
        .chain(POWER_EVENTS)
        .copied()
        .collect();
    both.sort();
    let mut record_only: Vec<&str> = WORDLESS.iter().chain(POWER).copied().collect();
    record_only.sort();

    assert_eq!(
        parse_sessions(both, None),
        parse_sessions(record_only, None)
    );
}

/// `POWER_UNATTENDED` opens no span: every `seconds` is 0.
#[test]
fn an_unattended_wake_bounds_nothing() {
    let mut all: Vec<&str> = WORDLESS.iter().chain(POWER_UNATTENDED).copied().collect();
    all.sort();
    let out = parse_sessions(all, None);
    assert!(out.iter().all(|s| s.seconds == 0));
}

/// `WORDLESS` with no power line: every `seconds` is 0.
#[test]
fn a_book_the_device_never_times_is_not_guessed_at_without_a_bound() {
    let out = parse_sessions(WORDLESS.iter().copied(), None);
    assert!(out.iter().all(|s| s.seconds == 0));
}

/// `CORRETTO` with `POWER` keeps `seconds` 462 and `Measure::Counted`.
#[test]
fn a_counted_sitting_is_untouched_by_the_power_records() {
    let mut all: Vec<&str> = CORRETTO.iter().chain(POWER).copied().collect();
    all.sort();
    let out = parse_sessions(all, None);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].seconds, 462, "TotalTime, not the ACTIVE span");
    assert_eq!(out[0].measure, Measure::Counted);
}

#[test]
fn a_named_page_event_is_read_exactly_as_before() {
    let out = parse_sessions(CVM.iter().copied(), None);
    assert_eq!(out.len(), 1);
    // 7431463 - 7390020, to the second.
    assert_eq!(out[0].seconds, 41);
    assert_eq!(out[0].words, 294);
    // EndPos 148207, not the NextTOCEntryPosition 56499.
    assert_eq!(out[0].end_position, 148_207);
    assert_eq!(out[0].page_turns, 2);
}
