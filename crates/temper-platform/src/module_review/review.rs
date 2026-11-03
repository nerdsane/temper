//! Reviewing one module: rate every item in each window, then ask where each found item shows (a part, then a line).
//! Jev rates and chooses; it writes nothing, so each finding's words are its item's own with the app's names and the
//! line filled in.

use futures::future::{join_all, try_join_all};

use super::excerpt::excerpt;
use super::items::{FOUND_AT, ITEMS, Item, LARGE_LINES};
use super::questions::{
    item_questions, line_key, line_questions, review_state, runners, runs_from, where_key,
    where_questions,
};
use super::source::{Line, Part, lines_of, parts_of, said, windows_of};
use super::types::{
    Answers, Finding, Judge, JudgeUnavailable, ModuleReview, ModuleSource, SpecSource,
};

/// An item Jev rated at or above [`FOUND_AT`], and the window where it rated highest, which is where it is looked for.
struct Found {
    item: &'static Item,
    rating: f64,
    window: usize,
}

/// Review one module against the specs of its app.
///
/// # Errors
///
/// [`JudgeUnavailable`] when Jev gave no rating. Once the items are rated, a Jev that does not say where one shows
/// leaves that finding without a line rather than failing the review.
pub async fn review_module(
    judge: &dyn Judge,
    module: &ModuleSource,
    specs: &[SpecSource],
) -> Result<ModuleReview, JudgeUnavailable> {
    let lines = lines_of(&module.source);
    let runners = runners(&module.module, specs);
    let read = excerpt(&module.module, specs);
    let windows = windows_of(&lines);
    let states: Vec<String> = windows
        .iter()
        .map(|window| review_state(&module.module, window, &read, lines.len()))
        .collect();
    debug_assert_eq!(states.len(), windows.len());

    let questions = item_questions();
    let ratings = try_join_all(
        states
            .iter()
            .map(|state| judge.ask(&module.module, state, &questions)),
    )
    .await?;
    let found = found_items(&ratings);
    let (placed, placing_tokens) = place(judge, &module.module, &windows, &states, &found).await;

    let runs = runs_from(&runners);
    let mut findings: Vec<Finding> = found
        .iter()
        .map(|found| {
            let line = placed
                .iter()
                .find(|(number, _)| *number == found.item.number)
                .map(|(_, line)| line);
            finding(found.item, found.rating, &module.path, line, &runs)
        })
        .collect();
    let size = said(&lines);
    if size > LARGE_LINES {
        findings.push(large(size, &lines, &module.path, &runs));
    }
    let input_tokens = ratings
        .iter()
        .map(|answers| answers.input_tokens)
        .sum::<u64>()
        + placing_tokens;
    Ok(ModuleReview {
        module: module.module.clone(),
        findings,
        input_tokens,
        left_out: read.left_out,
    })
}

/// The items rated at or above [`FOUND_AT`] in any window, each with its highest rating and that window.
fn found_items(ratings: &[Answers]) -> Vec<Found> {
    ITEMS
        .iter()
        .filter_map(|item| {
            let (window, rating) = ratings
                .iter()
                .enumerate()
                .filter_map(|(window, answers)| Some((window, answers.rating(item.kind)?)))
                .max_by(|a, b| a.1.total_cmp(&b.1))?;
            (rating >= FOUND_AT).then_some(Found {
                item,
                rating,
                window,
            })
        })
        .collect()
}

/// The line where each found item shows, by item number, and the tokens asking cost. Windows are asked at once.
async fn place(
    judge: &dyn Judge,
    module: &str,
    windows: &[Vec<Line>],
    states: &[String],
    found: &[Found],
) -> (Vec<(u8, Line)>, u64) {
    let asks = windows.iter().enumerate().filter_map(|(index, window)| {
        let here: Vec<&'static Item> = found
            .iter()
            .filter(|f| f.window == index)
            .map(|f| f.item)
            .collect();
        (!here.is_empty()).then(|| place_in_window(judge, module, window, &states[index], here))
    });
    let mut placed = Vec::new();
    let mut tokens = 0;
    for (lines, cost) in join_all(asks).await {
        placed.extend(lines);
        tokens += cost;
    }
    (placed, tokens)
}

/// Which part of one window shows each item, then which line of that part. A Jev that stops answering here leaves the
/// items unplaced: they are known, only where they show is not, and a guessed line would mislead.
async fn place_in_window(
    judge: &dyn Judge,
    module: &str,
    window: &[Line],
    state: &str,
    items: Vec<&'static Item>,
) -> (Vec<(u8, Line)>, u64) {
    let parts = parts_of(window);
    let Some(first_part) = parts.first() else {
        return (Vec::new(), 0);
    };
    let mut tokens = 0;
    let where_answers = if parts.len() > 1 {
        match judge
            .ask(module, state, &where_questions(&items, &parts))
            .await
        {
            Ok(answers) => answers,
            Err(_) => return (Vec::new(), 0),
        }
    } else {
        Answers::default()
    };
    tokens += where_answers.input_tokens;
    let chosen: Vec<(&Item, &Part)> = items
        .iter()
        .map(|item| {
            let key = where_answers.choice(&where_key(item));
            let part = parts
                .iter()
                .find(|part| Some(part.key.as_str()) == key)
                .unwrap_or(first_part);
            (*item, part)
        })
        .collect();
    let to_choose: Vec<(&Item, &Part)> = chosen
        .iter()
        .copied()
        .filter(|(_, part)| part.lines.len() > 1)
        .collect();
    let line_answers = if to_choose.is_empty() {
        Answers::default()
    } else {
        match judge.ask(module, state, &line_questions(&to_choose)).await {
            Ok(answers) => answers,
            Err(_) => return (Vec::new(), tokens),
        }
    };
    tokens += line_answers.input_tokens;
    let placed = chosen
        .iter()
        .filter_map(|(item, part)| {
            let key = line_answers.choice(&line_key(item));
            let line = part
                .lines
                .iter()
                .find(|line| Some(format!("L{}", line.number).as_str()) == key);
            Some((item.number, line.or(part.lines.first())?.clone()))
        })
        .collect();
    (placed, tokens)
}

fn finding(item: &Item, rating: f64, path: &str, line: Option<&Line>, runs: &str) -> Finding {
    debug_assert!((0.0..=1.0).contains(&rating), "a rating is 0..1");
    Finding {
        item: item.number,
        kind: item.kind.to_string(),
        title: item.title.to_string(),
        rating: (rating * 100.0).round() / 100.0,
        location: line.map_or_else(
            || path.to_string(),
            |line| format!("{path}:{}", line.number),
        ),
        line: line.map(|line| line.text.trim().to_string()),
        why: format!("In this app the module runs from {runs}; {}.", item.why),
        instead: format!("Instead: {}.", item.instead),
    }
}

/// Item 7 by size: a module this long is read as several concerns until its author says why it is not states.
fn large(size: usize, lines: &[Line], path: &str, runs: &str) -> Finding {
    let several_concerns = &ITEMS[6];
    debug_assert_eq!(several_concerns.number, 7);
    let branches = lines.iter().filter(|line| branches_on(&line.text)).count();
    Finding {
        kind: "large_module".to_string(),
        title: "A large module".to_string(),
        why: format!(
            "It is {size} lines with {branches} branches; a module this long almost always holds steps and decisions \
             the record's states should show."
        ),
        instead: format!(
            "Instead: {}, or say why it can't be split into states.",
            several_concerns.instead
        ),
        ..finding(several_concerns, 1.0, path, None, runs)
    }
}

/// Whether a line branches or loops (`if`, `match`, `while`, `loop`, `for` as words).
fn branches_on(text: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| matches!(word, "if" | "match" | "while" | "loop" | "for"))
}
