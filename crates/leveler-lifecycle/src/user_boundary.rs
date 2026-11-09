//! Explicit user execution boundaries.
//!
//! A model going quiet with an open plan is normally a contradiction worth one
//! lifecycle continuation. It is not a contradiction when the user themselves
//! bounded the authorized work: "finish stage 1, then wait for my confirmation"
//! authorizes stage 1 and withholds stage 2. The harness must not pressure the
//! model past a boundary the person set.
//!
//! This module reads that boundary from the user's OWN words. It is narrowly
//! scoped to an explicit stop/wait directive:
//!
//! - a directive that says stop/wait (`停下`, `暂停`, `等我确认`, `stop after`,
//!   `wait for me`, ...), and
//! - the stage named before the directive when the directive is conditional
//!   ("阶段 1 完成后停下" / "stop after phase 1").
//!
//! It does NOT scan for the topic of the request, does not judge whether the
//! work looks complete, and never accepts a model's claim that it needs
//! confirmation. Negated directives ("一次做完，中间不要停下来等我确认") are
//! explicitly not boundaries.
//!
//! The result is an authorization fact, not a completion verdict: a reached
//! boundary means the turn may end without another plan continuation. It never
//! marks a step done, never closes a Goal, and never turns the plan into an
//! acceptance authority.

use crate::plan::PlanState;

/// What the user's explicit stop directive authorizes before the pause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserPauseBoundary {
    /// Stop now. The user did not tie the pause to a named stage.
    Now,
    /// Stop once at least one step is completed and nothing is in progress.
    ///
    /// Used when the directive says "after you finish" but names no stage, so
    /// the plan has to demonstrate the authorized prefix instead.
    AfterAny,
    /// Stop after every plan step mentioning these named stages is completed.
    ///
    /// The markers are canonical (`stage1`, `step2`) and are matched against
    /// stage markers extracted from the plan text, never by similarity.
    AfterStages(Vec<String>),
}

impl UserPauseBoundary {
    /// Whether the model's own latest plan has reached the authorized boundary.
    ///
    /// A named-stage boundary is satisfied only when the plan actually names
    /// that stage and every such step is `completed`. If the stage cannot be
    /// located in the plan, the runtime does not guess: the boundary is not
    /// reached and the normal open-plan continuation still applies.
    pub fn reached(&self, plan: &PlanState) -> bool {
        self.reached_steps(
            plan.steps
                .iter()
                .map(|step| (step.step.as_str(), step.status.as_str())),
        )
    }

    /// The same reading over projected `(text, status)` pairs.
    ///
    /// A client that renders the plan can use this to present the same
    /// authority the runtime used, without inventing a second pause state.
    /// It is still authorization, not completion: the caller must not promote
    /// a reached boundary into "task done".
    pub fn reached_steps<'a, I>(&self, steps: I) -> bool
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let steps: Vec<(&str, &str)> = steps.into_iter().collect();
        let any_active = steps.iter().any(|(_, status)| *status == "in_progress");
        match self {
            Self::Now => true,
            Self::AfterAny => !any_active && steps.iter().any(|(_, status)| *status == "completed"),
            Self::AfterStages(markers) => {
                if markers.is_empty() {
                    return false;
                }
                // Only the named, authorized stages decide the boundary. An
                // unauthorized later step that the model already started does
                // not buy more pressure past the user's stop: the pause was
                // reached when the authorized prefix finished.
                //
                // A step that IS the boundary ("stage 1 done, then wait for
                // confirmation") is not evidence about the work and is left
                // out: the boundary can be the model's current step, and
                // counting it as unfinished work would make the user's own
                // pause unreachable.
                let named: Vec<(&str, &str)> = steps
                    .iter()
                    .copied()
                    .filter(|(text, _)| {
                        let found = extract_stage_markers(text);
                        markers.iter().any(|marker| found.contains(marker))
                    })
                    .collect();
                let work: Vec<(&str, &str)> = named
                    .iter()
                    .copied()
                    .filter(|(text, _)| !is_boundary_step(text))
                    .collect();
                // The stage's WORK decides when the plan has any. A plan that
                // folds "…then pause for confirmation" into its one stage step
                // has no separate work row; there the boundary step can only
                // witness the pause when the model declared it completed.
                let decisive = if work.is_empty() { &named } else { &work };
                !decisive.is_empty() && decisive.iter().all(|(_, status)| *status == "completed")
            }
        }
    }
}

/// Read an explicit user pause boundary from one user-authored message.
///
/// Later sentences can replace an earlier directive. A negated stop ("do not
/// stop") clears the boundary; a more specific stage boundary outranks a
/// generic one. Returns `None` when the user did not bound the work.
pub fn extract_user_pause(text: &str) -> Option<UserPauseBoundary> {
    let mut boundary: Option<UserPauseBoundary> = None;
    for sentence in split_sentences(text) {
        // A release is scoped to the clause that states it. The newest
        // statement wins: text after the last release clause may state a new
        // boundary (`不需要等我确认，但阶段一结束后要暂停`), and that boundary is
        // the user's actual instruction.
        let examined = match release_cut(sentence) {
            Some(cut) => {
                boundary = None;
                &sentence[cut..]
            }
            None => sentence,
        };
        if let Some(found) = pause_in_sentence(examined) {
            boundary = Some(match boundary {
                Some(existing) => merge_boundaries(existing, found),
                None => found,
            });
        }
    }
    boundary
}

/// Byte offset just past the last clause in `sentence` that releases a pause.
fn release_cut(sentence: &str) -> Option<usize> {
    let mut cut = None;
    let mut offset = 0;
    for part in sentence.split_inclusive(['，', ',', '；', ';', '：', ':']) {
        let clause = part.trim_end_matches(['，', ',', '；', ';', '：', ':', ' ', '\t']);
        if releases_pause(clause) {
            cut = Some(offset + part.len());
        }
        offset += part.len();
    }
    cut
}

/// Whether a plan step is the user's boundary itself rather than work.
///
/// A model often keeps "stage 1 done, then wait for confirmation" as its own
/// step, and may leave it `in_progress` while it waits. That step is a reading
/// of the user's authorization, not evidence that stage 1's work is unfinished:
/// counting it would make the user's own pause unreachable.
///
/// The markers are about waiting for the USER, not about verifying output
/// ("run cat to confirm the content" is work and must stay counted).
fn is_boundary_step(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "等待用户",
        "等待你",
        "等待我",
        "等我",
        "等待确认",
        "用户确认",
        "确认后再",
        "再继续",
        "暂停",
        "停下",
        "wait for the user",
        "wait for you",
        "wait for me",
        "wait for confirmation",
        "await confirmation",
        "pause",
        "stop here",
    ];
    let lower = text.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Whether this user message explicitly pauses work the plan has reached.
pub fn user_pause_reached(text: &str, plan: &PlanState) -> bool {
    extract_user_pause(text).is_some_and(|boundary| boundary.reached(plan))
}

/// Whether this user message explicitly authorizes continuing work an earlier
/// message paused.
///
/// Only the user's OWN directive counts, and only as a directive: the
/// continuation phrase has to start a sentence (an acknowledgement such as
/// `确认` or `好的` may precede it), or the message has to explicitly release
/// the wait (`不用再等`, `无需确认`). A status question that merely mentions
/// continuing is not authorization.
pub fn authorizes_continuation(text: &str) -> bool {
    for sentence in split_sentences(text) {
        if releases_pause(sentence) {
            return true;
        }
        if starts_with_continuation_cue(strip_acknowledgements(sentence)) {
            return true;
        }
    }
    false
}

/// The newest authorization decision the user's own instructions contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationSignal {
    /// The user stated a stop/wait boundary.
    Pause,
    /// The user authorized continuing the paused work.
    Continue,
}

/// The newest authorization signal in a recorded user-instruction sequence.
///
/// `None` means the sequence states neither: a goal that was never paused, or a
/// paused goal whose durable objective/amendments do not carry the user's own
/// pause wording. It is never read as a release.
pub fn latest_authorization_signal<'a, I>(texts: I) -> Option<AuthorizationSignal>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut current = None;
    for text in texts {
        if extract_user_pause(text).is_some() {
            current = Some(AuthorizationSignal::Pause);
        } else if authorizes_continuation(text) {
            current = Some(AuthorizationSignal::Continue);
        }
    }
    current
}

/// Whether a DURABLE pause is still in force after the user's later
/// instructions.
///
/// The stored column records the boundary an earlier window reached. It is
/// cleared at the END of a window, so during the window the user just
/// authorized it is still set. Reading the column alone would let the harness
/// cut that authorized window short at its first quiet round and re-pause a
/// goal the user has already released — the user's own newest directive is the
/// authority here.
///
/// Only an explicit continuation releases it. A message that states neither
/// (a status question, a supplement on the current work) leaves the recorded
/// pause in force, and a pause the user stated again keeps it: forgetting a
/// boundary is the one error this must not make.
pub fn pause_in_force<'a, I>(durable: bool, texts: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    durable
        && !matches!(
            latest_authorization_signal(texts),
            Some(AuthorizationSignal::Continue)
        )
}

/// The pause boundary a sequence of user-authored instructions leaves in
/// force, in order.
///
/// A later pause replaces an earlier one; a later continuation authorization
/// releases it; an instruction that does neither (a status question, a
/// supplement on the current work) leaves the previous boundary untouched.
/// This is what makes the user's boundary survive messages that are not about
/// the authorization — a Goal turn does not lose its pause because the person
/// asked which files changed.
///
/// The caller supplies the user's own texts in their recorded order (the
/// goal's objective plus its amendments, or one turn's objective anchor).
pub fn effective_user_pause<'a, I>(texts: I) -> Option<UserPauseBoundary>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut current = None;
    for text in texts {
        if let Some(found) = extract_user_pause(text) {
            current = Some(found);
        } else if authorizes_continuation(text) {
            current = None;
        }
    }
    current
}

/// Acknowledgements a person puts in front of the actual instruction.
const CONTINUATION_ACKNOWLEDGEMENTS: &[&str] = &[
    "好的",
    "好",
    "确认",
    "确认一下",
    "明白了",
    "收到",
    "没问题",
    "可以",
    "行",
    "是的",
    "对的",
    "okay",
    "ok",
    "yes",
    "sure",
    "alright",
    "all right",
    "go ahead",
];

/// Phrases that instruct the runtime to continue the paused work.
const CONTINUATION_CUES: &[&str] = &[
    "继续", "接着", "往下", "go on", "continue", "resume", "proceed", "carry on",
];

fn strip_acknowledgements(sentence: &str) -> &str {
    let mut rest = sentence.trim_start_matches(is_clause_punctuation_or_space);
    loop {
        let before = rest;
        for acknowledgement in CONTINUATION_ACKNOWLEDGEMENTS {
            let Some(tail) = rest.strip_prefix(acknowledgement) else {
                continue;
            };
            let Some(next) = tail.chars().next() else {
                return "";
            };
            if is_clause_punctuation_or_space(next) {
                rest = tail.trim_start_matches(is_clause_punctuation_or_space);
                break;
            }
            // `确认继续阶段 2` states the acknowledgement and the directive in
            // one breath, with no punctuation between them.
            if starts_with_continuation_cue(tail) {
                rest = tail;
                break;
            }
        }
        if rest == before {
            return rest;
        }
    }
}

fn starts_with_continuation_cue(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    CONTINUATION_CUES.iter().any(|cue| lower.starts_with(cue))
}

fn is_clause_punctuation_or_space(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '，' | '。'
                | '、'
                | '；'
                | '：'
                | '！'
                | '？'
                | ','
                | '.'
                | ';'
                | ':'
                | '!'
                | '?'
                | '~'
        )
}

fn merge_boundaries(existing: UserPauseBoundary, found: UserPauseBoundary) -> UserPauseBoundary {
    match (existing, found) {
        (UserPauseBoundary::AfterStages(mut a), UserPauseBoundary::AfterStages(b)) => {
            for marker in b {
                if !a.contains(&marker) {
                    a.push(marker);
                }
            }
            UserPauseBoundary::AfterStages(a)
        }
        (existing @ UserPauseBoundary::AfterStages(_), _) => existing,
        (_, found @ UserPauseBoundary::AfterStages(_)) => found,
        (existing @ UserPauseBoundary::AfterAny, _) => existing,
        (_, found @ UserPauseBoundary::AfterAny) => found,
        (existing, _) => existing,
    }
}

fn pause_in_sentence(sentence: &str) -> Option<UserPauseBoundary> {
    let cue = first_positive_pause_cue(sentence)?;
    let before = &sentence[..cue];
    let completed_stages = completed_stage_markers(before);
    if !completed_stages.is_empty() {
        return Some(UserPauseBoundary::AfterStages(completed_stages));
    }
    if !extract_stage_markers(sentence).is_empty() || has_completion_cue(before) {
        return Some(UserPauseBoundary::AfterAny);
    }
    Some(UserPauseBoundary::Now)
}

/// Byte offset of the first non-negated pause cue in the sentence.
fn first_positive_pause_cue(sentence: &str) -> Option<usize> {
    const CUES: &[&str] = &[
        "等我看到",
        "等我确认",
        "等我回复",
        "等待确认",
        "等待我",
        "等我",
        "确认后再",
        "确认之后",
        "停下来",
        "先停",
        "停下",
        "暂停",
        "结束本轮",
        "结束这一轮",
        "停止本轮",
        "stop and wait",
        "then stop",
        "then pause",
        "do not continue until",
        "don't continue until",
        "wait until i",
        "wait until you",
        "wait for my",
        "wait for me",
        "stop after",
        "stop when",
        "stop here",
        "stop now",
        "pause after",
        "pause here",
        "hold off",
        "end this turn",
        "end the turn",
    ];
    let lower = sentence.to_ascii_lowercase();
    let mut best: Option<usize> = None;
    for cue in CUES {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(cue) {
            let at = from + rel;
            // `lower` is ASCII-lowercased per char; Chinese is unchanged and
            // ASCII case folding preserves byte offsets.
            if !cue_negated(&lower[..at]) {
                best = Some(best.map_or(at, |current| current.min(at)));
                break;
            }
            from = at + cue.len();
        }
    }
    best
}

/// A negation that governs the pause cue.
///
/// A negation word scopes over the modifiers before the cue, not only the token
/// immediately before it: `不需要逐步等待确认` and `不用每一步都等我确认` keep the
/// model working, and reading the pause word while losing the negation ended
/// the window at a boundary the user never set. The scope ends at the latest
/// clause separator, and only pause modifiers may sit between the negation and
/// the cue: a predicate (`不要忘记等待确认`) keeps the pause, because the user is
/// reminding the model to wait, not cancelling the wait.
fn cue_negated(before: &str) -> bool {
    // Only the clause that contains the cue participates: a negation in an
    // earlier clause says nothing about this one.
    let clause = before
        .rsplit(['，', ',', '；', ';', '。', '！', '!', '？', '?', '：', ':'])
        .next()
        .unwrap_or(before);
    let clause = clause.trim_end_matches([' ', '\t']).to_ascii_lowercase();
    let Some((negation, gap)) = last_negation_scope(&clause) else {
        return false;
    };
    // A contrast starts a new statement; the negation no longer governs it.
    if starts_contrast(gap, negation) {
        return false;
    }
    // `不需要逐步等待确认` negates the wait; `不要忘记等待确认` does not. Only
    // modifiers may separate the negation from the cue.
    only_pause_modifiers(gap)
}

/// Adverbs a person puts between the negation and the cue. They do not change
/// which verb is negated: `不需要逐步等待确认` means `不需要等待确认`.
const PAUSE_MODIFIERS: &[&str] = &[
    "每一步都",
    "每次都",
    "每步都",
    "每一步",
    "每步",
    "逐步",
    "逐个",
    "逐一",
    "一一",
    "每次",
    "中途",
    "途中",
    "一直",
    "反复",
    "继续",
    "再",
    "都",
];

/// Whether the whole gap consists of pause modifiers.
fn only_pause_modifiers(gap: &str) -> bool {
    let mut rest = gap.trim();
    while !rest.is_empty() {
        let Some(modifier) = PAUSE_MODIFIERS
            .iter()
            .filter(|modifier| rest.starts_with(**modifier))
            .max_by_key(|modifier| modifier.len())
        else {
            return false;
        };
        rest = rest[modifier.len()..].trim_start();
    }
    true
}

/// The last negation word in a clause and the text between its end and the cue.
fn last_negation_scope(clause: &str) -> Option<(&str, &str)> {
    const NEGATIONS: &[&str] = &[
        "不需要",
        "不用",
        "不必",
        "无需",
        "无须",
        "毋须",
        "没必要",
        "用不着",
        "不要",
        "不允许",
        "不能",
        "不可以",
        "不准",
        "不得",
        "不该",
        "不应",
        "别",
        "勿",
        "甭",
        "禁止",
        "do not",
        "don't",
        "dont",
        "never",
        "without",
        "no need to",
        "not",
        "不",
    ];
    let mut best: Option<(usize, usize, usize)> = None;
    for negation in NEGATIONS {
        let mut from = 0;
        while let Some(offset) = clause[from..].find(negation) {
            let start = from + offset;
            from = start + negation.len();
            if !negation_stands_alone(clause, start, negation) {
                continue;
            }
            if best.is_none_or(|(current, current_end, _)| {
                start > current || (start == current && start + negation.len() > current_end)
            }) {
                best = Some((start, start + negation.len(), negation.len()));
            }
        }
    }
    let (start, end, _) = best?;
    Some((&clause[start..end], &clause[end..]))
}

/// Whether an occurrence is the negation word itself, not a particle inside
/// another word (`特别`, `区别`, `类别`, `cannot`, `annotation`).
fn negation_stands_alone(clause: &str, start: usize, negation: &str) -> bool {
    if negation.is_ascii() {
        return !clause[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    }
    if !matches!(negation, "别" | "勿" | "甭") {
        return true;
    }
    let before = clause[..start].chars().next_back();
    !before.is_some_and(|c| {
        matches!(
            c,
            '特' | '区' | '分' | '类' | '级' | '告' | '个' | '性' | '派'
        )
    })
}

/// Whether a contrast word sits between the negation and the cue.
fn starts_contrast(gap: &str, negation: &str) -> bool {
    if ["但", "可是", "不过", "然而", "而是", "却"]
        .iter()
        .any(|marker| gap.contains(marker))
    {
        return true;
    }
    let padded = format!(" {gap} ");
    [" but ", " however ", " yet ", " though ", " instead "]
        .iter()
        .any(|marker| padded.contains(marker))
        || (negation.is_ascii() && gap.trim_start().starts_with("but "))
}

/// A whole-sentence negation of stopping: "一次做完，中间不要停下来等我确认".
fn releases_pause(sentence: &str) -> bool {
    let normalized = sentence
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    [
        "不要停",
        "不用停",
        "不必停",
        "别停",
        "不要暂停",
        "不用暂停",
        "别暂停",
        "不要等",
        "不用等",
        "不用再等",
        "无需再等",
        "不必再等",
        "不必等",
        "别等",
        "无需等",
        "无需确认",
        "不用再确认",
        "不需要等",
        "donotstop",
        "dontstop",
        "donotwait",
        "dontwait",
        "donotwaitforme",
        "noneedtowait",
        "noneedtoconfirm",
        "withoutstopping",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

fn has_completion_cue(text: &str) -> bool {
    let normalized = text
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    [
        "做完",
        "完成后",
        "完成之后",
        "完成时",
        "结束后",
        "结束之后",
        "交付完",
        "写完后",
        "实现完",
        "跑完",
        "通过后",
        "之后",
        "以后",
        "然后停",
        "再停",
        "done",
        "finished",
        "finish",
        "complete",
        "afterfinishing",
        "afteryoufinish",
        "whenfinished",
        "oncefinished",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

/// Stage markers in the clauses of `before` that also state completion.
fn completed_stage_markers(before: &str) -> Vec<String> {
    let mut markers = Vec::new();
    for clause in before.split(['，', ',', '；', ';', '。', '！', '!', '？', '?', '\n']) {
        if !has_completion_cue(clause) {
            continue;
        }
        for marker in extract_stage_markers(clause) {
            if !markers.contains(&marker) {
                markers.push(marker);
            }
        }
    }
    markers
}

fn split_sentences(text: &str) -> impl Iterator<Item = &str> {
    text.split(['。', '！', '!', '？', '?', '\n', '；', ';'])
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
}

/// Canonical stage markers named in `text`: `stage1`, `step2`, ...
///
/// `阶段 1`, `第一阶段`, `Phase 1` and `stage 1` all canonicalize to `stage1`;
/// `第 2 步` and `step 2` canonicalize to `step2`. This is exact marker
/// identity, not text similarity: the plan must name the same stage number.
pub(crate) fn extract_stage_markers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let mut markers = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let mut consumed = None;
        if starts_with(&chars, index, "阶段") {
            if let Some((number, end)) = number_after(&chars, index + 2) {
                push_marker(&mut markers, "stage", &number);
                consumed = Some(end);
            } else if let Some(number) = number_before(&chars, index) {
                push_marker(&mut markers, "stage", &number);
            }
        } else if chars[index] == '步' {
            if let Some(number) = number_before(&chars, index) {
                push_marker(&mut markers, "step", &number);
            }
        } else if starts_with_word(&lower, index, "phase") {
            if let Some((number, end)) = number_after(&lower, index + "phase".len()) {
                push_marker(&mut markers, "stage", &number);
                consumed = Some(end);
            }
        } else if starts_with_word(&lower, index, "stage") {
            if let Some((number, end)) = number_after(&lower, index + "stage".len()) {
                push_marker(&mut markers, "stage", &number);
                consumed = Some(end);
            }
        } else if starts_with_word(&lower, index, "step")
            && let Some((number, end)) = number_after(&lower, index + "step".len())
        {
            push_marker(&mut markers, "step", &number);
            consumed = Some(end);
        }
        index = consumed.unwrap_or(index + 1);
    }
    markers
}

fn push_marker(markers: &mut Vec<String>, kind: &str, number: &str) {
    let marker = format!("{kind}{number}");
    if !markers.contains(&marker) {
        markers.push(marker);
    }
}

fn starts_with(chars: &[char], index: usize, needle: &str) -> bool {
    let needle: Vec<char> = needle.chars().collect();
    chars
        .get(index..index + needle.len())
        .is_some_and(|slice| slice == needle.as_slice())
}

fn starts_with_word(chars: &[char], index: usize, needle: &str) -> bool {
    if !starts_with(chars, index, needle) {
        return false;
    }
    let before_is_word = index
        .checked_sub(1)
        .and_then(|i| chars.get(i))
        .is_some_and(|c| c.is_ascii_alphanumeric());
    let after = chars.get(index + needle.chars().count());
    !before_is_word && after.is_none_or(|c| !c.is_ascii_alphanumeric())
}

fn number_after(chars: &[char], from: usize) -> Option<(String, usize)> {
    let mut index = from;
    while chars
        .get(index)
        .is_some_and(|c| c.is_whitespace() || matches!(c, ':' | '：' | '#'))
    {
        index += 1;
    }
    let start = index;
    while chars.get(index).is_some_and(char::is_ascii_digit) {
        index += 1;
    }
    if index > start {
        return Some((chars[start..index].iter().collect(), index));
    }
    let numeral = chinese_numeral(*chars.get(index)?)?;
    Some((numeral.to_string(), index + 1))
}

fn number_before(chars: &[char], at: usize) -> Option<String> {
    let mut index = at;
    while index > 0 && (chars[index - 1].is_whitespace() || chars[index - 1] == '第') {
        index -= 1;
    }
    let previous = *chars.get(index.checked_sub(1)?)?;
    if previous.is_ascii_digit() {
        let mut start = index - 1;
        while start > 0 && chars[start - 1].is_ascii_digit() {
            start -= 1;
        }
        return Some(chars[start..index].iter().collect());
    }
    chinese_numeral(previous).map(str::to_string)
}

fn chinese_numeral(c: char) -> Option<&'static str> {
    Some(match c {
        '一' => "1",
        '二' | '两' => "2",
        '三' => "3",
        '四' => "4",
        '五' => "5",
        '六' => "6",
        '七' => "7",
        '八' => "8",
        '九' => "9",
        '十' => "10",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanState, PlanStep};

    fn step(text: &str, status: &str) -> PlanStep {
        PlanStep {
            step: text.into(),
            status: status.into(),
            id: None,
            origin: Default::default(),
        }
    }

    fn plan(steps: Vec<PlanStep>) -> PlanState {
        PlanState { steps }
    }

    #[test]
    fn a_stage_boundary_is_read_from_the_users_own_words() {
        let boundary = extract_user_pause("阶段 1 完成后停下，等待我确认再执行阶段 2。").unwrap();
        assert_eq!(
            boundary,
            UserPauseBoundary::AfterStages(vec!["stage1".into()])
        );
    }

    #[test]
    fn a_chinese_numeral_stage_matches_a_digit_plan_step() {
        let boundary = extract_user_pause("第一阶段做完之后先停下，等我看到小结再继续。").unwrap();
        let plan = plan(vec![
            step("阶段 1：实现接口", "completed"),
            step("阶段 2：写测试", "pending"),
        ]);
        assert!(boundary.reached(&plan), "{boundary:?}");
    }

    #[test]
    fn an_unfinished_named_stage_does_not_reach_the_boundary() {
        let boundary = extract_user_pause("阶段 1 完成后停下，等待我确认再执行阶段 2。").unwrap();
        let plan = plan(vec![
            step("阶段 1：实现接口", "in_progress"),
            step("阶段 2：写测试", "pending"),
        ]);
        assert!(
            !boundary.reached(&plan),
            "stage 1 is not finished, so the authorized work is still open"
        );
    }

    #[test]
    fn a_started_unauthorized_stage_does_not_undo_the_pause() {
        let boundary = extract_user_pause("阶段 1 完成后停下，等待我确认再执行阶段 2。").unwrap();
        let plan = plan(vec![
            step("阶段 1：实现接口", "completed"),
            step("阶段 2：写测试", "in_progress"),
        ]);
        assert!(
            boundary.reached(&plan),
            "the authorized prefix is finished; a later started step does not buy pressure"
        );
    }

    #[test]
    fn a_plan_that_does_not_name_the_stage_does_not_guess() {
        let boundary = extract_user_pause("阶段 1 完成后停下，等待我确认再执行阶段 2。").unwrap();
        let plan = plan(vec![step("实现接口", "completed")]);
        assert!(
            !boundary.reached(&plan),
            "an unlocatable stage boundary is not assumed reached"
        );
    }

    #[test]
    fn do_not_stop_is_not_a_pause() {
        assert_eq!(
            extract_user_pause("这是一个多阶段任务，请一次做完，中间不要停下来等我确认。"),
            None
        );
        assert_eq!(
            extract_user_pause("Please do it all in one go, do not stop and wait for me."),
            None
        );
    }

    /// A negation scopes over the rest of its clause, not just the token right
    /// before the cue. `不需要逐步等待确认` and `不用每一步都等我确认` authorize the
    /// model to continue; reading the pause word while losing the negation
    /// ended the work window early and skipped the continuation the user asked
    /// for.
    #[test]
    fn a_negated_wait_with_a_modifier_is_not_a_pause() {
        assert_eq!(
            extract_user_pause("直接基于真实代码推进开发，不需要逐步等待确认。"),
            None
        );
        assert_eq!(
            extract_user_pause("你不用每一步都等我确认，直接做完。"),
            None
        );
        assert_eq!(extract_user_pause("不必每次确认，连续执行完。"), None);
    }

    /// The modifiers a person actually puts between the negation and the cue
    /// do not turn the authorization into a pause.
    #[test]
    fn an_adverb_between_the_negation_and_the_cue_does_not_create_a_pause() {
        for sentence in [
            "不需要逐步等待确认。",
            "不用每次都等我确认。",
            "不要一直等待确认。",
            "不要逐个等待确认。",
            "不要中途停下来问我。",
            "不暂停，直接做完。",
        ] {
            assert_eq!(extract_user_pause(sentence), None, "`{sentence}`");
        }
    }

    /// `不要忘记…` is a reminder to wait, not a cancellation of the wait.
    #[test]
    fn a_reminder_to_wait_is_still_a_pause() {
        assert_eq!(
            extract_user_pause("不要忘记等待确认。"),
            Some(UserPauseBoundary::Now)
        );
        assert!(extract_user_pause("别忘了完成后等我确认。").is_some());
    }

    /// The English list is matched after trimming, so the negation before its
    /// cue must still count.
    #[test]
    fn english_negation_matches_before_its_cue() {
        assert!(cue_negated("do not "));
        assert!(cue_negated("please do not "));
        assert!(!cue_negated("do not forget "));
    }

    /// The same sentence must leave every closeout reader off: no boundary is
    /// reached, no effective pause exists, and the newest authorization signal
    /// is not `Pause`. The brief's sentence previously read as `Now`, which
    /// ended the authorized window before the unregistered-task continuation
    /// could run.
    #[test]
    fn a_negated_wait_does_not_reach_the_closeout_boundary() {
        let text = "直接基于真实代码推进开发，不需要逐步等待确认。";
        let plan = plan(vec![step("实现接口", "completed")]);
        assert!(!user_pause_reached(text, &plan));
        assert_eq!(effective_user_pause([text]), None);
        assert_eq!(latest_authorization_signal([text]), None);
    }

    /// The negation stops at a contrast and at a clause boundary: a real pause
    /// stated after `但` (or in the next clause) is still the user's boundary.
    #[test]
    fn a_negation_does_not_erase_a_pause_in_the_next_clause() {
        let boundary = extract_user_pause("不需要等我确认，但阶段一结束后要暂停。").unwrap();
        assert_eq!(
            boundary,
            UserPauseBoundary::AfterStages(vec!["stage1".into()])
        );
        let boundary = extract_user_pause("不要修改现有代码，完成后暂停。").unwrap();
        assert_eq!(boundary, UserPauseBoundary::AfterAny);
    }

    #[test]
    fn a_sequential_plan_is_not_a_pause() {
        assert_eq!(
            extract_user_pause("阶段 1 做完先把结果写出来，然后再进入阶段 2。"),
            None
        );
    }

    #[test]
    fn a_direct_stop_is_a_now_boundary() {
        let boundary = extract_user_pause("现在停下，不要继续了。").unwrap();
        assert_eq!(boundary, UserPauseBoundary::Now);
        assert!(boundary.reached(&plan(vec![step("anything", "pending")])));
    }

    #[test]
    fn an_unnamed_completion_boundary_needs_finished_progress() {
        let boundary = extract_user_pause("先实现接口，做完之后停下等我确认。").unwrap();
        assert_eq!(boundary, UserPauseBoundary::AfterAny);
        assert!(!boundary.reached(&plan(vec![step("implement", "in_progress")])));
        assert!(boundary.reached(&plan(vec![step("implement", "completed")])));
    }

    #[test]
    fn a_later_amendment_that_says_continue_releases_the_boundary() {
        // The caller reads the LATEST user-authored instruction, so a later
        // "继续阶段 2" message simply has no pause directive.
        assert_eq!(extract_user_pause("继续阶段 2，做完后跑测试。"), None);
    }

    /// The real-provider plan shape: stage 1's work is `done`, the model keeps
    /// the user's boundary as its own `in_progress` step while it waits. The
    /// pause IS reached — the boundary step is not unfinished work.
    #[test]
    fn the_pause_step_itself_does_not_hide_a_reached_boundary() {
        let boundary = extract_user_pause("阶段 1 完成后停下，等待我确认再执行阶段 2。").unwrap();
        let finished_plan = plan(vec![
            step("阶段 1：创建 stage-1.txt", "completed"),
            step("阶段 1：运行 cat 确认输出", "completed"),
            step("阶段 1 完成后停下，等待用户确认后再继续", "in_progress"),
            step("阶段 2：创建 stage-2.txt", "pending"),
        ]);
        assert!(
            boundary.reached(&finished_plan),
            "the user's own boundary is not evidence that stage 1 is unfinished"
        );
        // The other real-provider shape: ONE stage-1 step that folds in the
        // pause ("... then pause and wait for the user"). Completed = the
        // authorized stage is done.
        let folded = plan(vec![
            step(
                "阶段 1：创建 stage-1.txt，运行 cat 确认，然后暂停等待用户确认",
                "completed",
            ),
            step("阶段 2：创建 stage-2.txt", "pending"),
        ]);
        assert!(
            boundary.reached(&folded),
            "the completed stage step carries the pause with it"
        );
        let waiting_before_work = plan(vec![
            step("阶段 1：创建 stage-1.txt", "pending"),
            step("阶段 1 完成后停下，等待用户确认后再继续", "in_progress"),
        ]);
        assert!(
            !boundary.reached(&waiting_before_work),
            "an unfinished stage-1 work step still blocks the pause"
        );
        // A real stage-1 work step still in progress keeps the boundary open.
        let unfinished_plan = plan(vec![
            step("阶段 1：创建 stage-1.txt", "completed"),
            step("阶段 1：运行 cat 确认输出", "in_progress"),
            step("阶段 1 完成后停下，等待用户确认后再继续", "pending"),
        ]);
        assert!(
            !boundary.reached(&unfinished_plan),
            "a work step naming the authorized stage still blocks the pause"
        );
    }

    #[test]
    fn english_stage_markers_are_canonicalized() {
        let boundary = extract_user_pause("Finish phase 2, then stop and wait for me.").unwrap();
        assert_eq!(
            boundary,
            UserPauseBoundary::AfterStages(vec!["stage2".into()])
        );
    }

    /// The pause is a fact about the user's authorization, not about the
    /// newest sentence they typed. A status question after the pause changed
    /// nothing, so the boundary is still in force.
    #[test]
    fn a_status_question_does_not_release_the_pause() {
        let texts = [
            "阶段 1 完成后停下，等待我确认再执行阶段 2。",
            "刚才阶段 1 修改了哪些文件？测试通过了吗？",
            "现在进度怎么样？",
            "阶段 1 的测试结果是什么？",
        ];
        let boundary = effective_user_pause(texts).expect("the pause is still in force");
        assert_eq!(
            boundary,
            UserPauseBoundary::AfterStages(vec!["stage1".into()])
        );
    }

    /// A supplement on the CURRENT stage is new work inside the authorized
    /// range, not authorization for the withheld stage.
    #[test]
    fn a_supplement_on_the_current_stage_does_not_release_the_pause() {
        let texts = [
            "阶段 1 完成后停下，等待我确认再执行阶段 2。",
            "阶段 1 的函数再加一个空字符串边界检查。",
        ];
        assert!(effective_user_pause(texts).is_some());
    }

    /// The user's own continuation directive is what releases the pause —
    /// including the acknowledgement form the person actually types.
    #[test]
    fn an_explicit_continuation_authorization_releases_the_pause() {
        for release in [
            "继续阶段 2，把剩余任务完成并验收。",
            "确认，继续阶段 2，完成剩余工作和验收。",
            "继续执行剩余阶段，不用再等我确认。",
            "好的，继续。",
            "go on, finish stage 2.",
            "Continue with stage 2 and verify it.",
        ] {
            let texts = [
                "阶段 1 完成后停下，等待我确认再执行阶段 2。",
                "阶段 1 修改了哪些文件？",
                release,
            ];
            assert_eq!(
                effective_user_pause(texts),
                None,
                "`{release}` authorizes continuing the paused goal"
            );
        }
    }

    /// The durable column outlives the window that set it, so the reading has
    /// to fold in the user's newest directive. Without that, the window the
    /// user just authorized ends at its first quiet round as "paused" again.
    #[test]
    fn a_recorded_pause_is_released_by_the_users_own_continuation() {
        let objective = "阶段 1 完成后停下，等待我确认再执行阶段 2。";
        for release in [
            "继续阶段 2，把剩余任务完成并验收。",
            "确认，继续阶段 2。",
            "go on, finish stage 2.",
        ] {
            assert!(
                !pause_in_force(true, [objective, release]),
                "`{release}` releases the recorded pause"
            );
        }
        // Nothing about the authorization leaves the recorded pause in force.
        for neutral in [
            "阶段 1 修改了哪些文件？",
            "测试通过了吗？",
            "顺便把 stage-extra.txt 也补上。",
            "先停在这里，等我确认。",
        ] {
            assert!(
                pause_in_force(true, [objective, neutral]),
                "`{neutral}` does not release the pause"
            );
        }
        // No recorded pause means nothing to release.
        assert!(!pause_in_force(false, [objective]));
    }

    /// A question that merely mentions the word "continue" is not
    /// authorization: the directive has to be the user's instruction.
    #[test]
    fn a_question_that_mentions_continuing_is_not_authorization() {
        assert!(!authorizes_continuation("接下来还要继续多久？"));
        assert!(!authorizes_continuation("刚才阶段 1 修改了哪些文件？"));
        assert!(authorizes_continuation("继续阶段 2"));
        assert!(authorizes_continuation("确认，继续阶段 2"));
    }

    /// A continuation asked politely is still the user's authorization: the
    /// acknowledgement plus the directive is what releases the pause, whether
    /// or not the sentence ends with a question mark.
    #[test]
    fn a_polite_continuation_question_is_still_authorization() {
        for release in ["确认，继续阶段 2，可以吗？", "好的，继续吧。"] {
            assert!(
                authorizes_continuation(release),
                "`{release}` authorizes continuing"
            );
            let texts = ["阶段 1 完成后停下，等待我确认再执行阶段 2。", release];
            assert_eq!(
                effective_user_pause(texts),
                None,
                "`{release}` releases the recorded pause"
            );
        }
    }

    /// New work requested inside the paused window is a scope amendment, not
    /// an authorization to continue: adding a requirement never releases the
    /// user's own pause.
    #[test]
    fn a_scope_amendment_does_not_release_the_pause() {
        for amendment in [
            "能不能顺便把评论权限校验也补上？",
            "顺便再把 src/permissions.rs 的校验补上。",
            "阶段 1 再加一个空字符串边界检查。",
        ] {
            let texts = ["阶段 1 完成后停下，等待我确认再执行阶段 2。", amendment];
            assert!(
                effective_user_pause(texts).is_some(),
                "`{amendment}` adds work; it does not authorize stage 2"
            );
        }
    }
}
