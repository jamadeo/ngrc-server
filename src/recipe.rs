//! A deliberately small fallback parser for the no-key demo. Live mode lets the
//! model interpret the full text and submit a plan through the same validated tool.
use anyhow::{Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

pub const EXAMPLE: &str = "Lemon couscous\n\nIngredients\n1 cup couscous\n1 cup water or vegetable stock\n1 tablespoon olive oil\n1 lemon\nSalt to taste\n\nInstructions\n1. Measure the couscous into a heatproof bowl. Have a lid or plate ready.\n2. Bring the water or stock to a boil in a saucepan. Carefully pour it over the couscous, add the olive oil and a pinch of salt, and cover. Let stand for 5 minutes.\n3. Remove the lid carefully, fluff with a fork and check that the grains are tender. If needed, follow your couscous package directions for additional hydration.\n4. Add lemon zest and juice to taste. Serve and enjoy.";
pub const MAX_RECIPE_BYTES: usize = 32_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerSpec {
    pub label: String,
    pub seconds: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeStep {
    pub instruction: String,
    pub timers: Vec<TimerSpec>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub title: String,
    pub ingredients: Vec<String>,
    pub steps: Vec<RecipeStep>,
    pub notes: Vec<String>,
}
impl Plan {
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() || self.title.len() > 200 {
            bail!("title must be 1–200 bytes");
        }
        if self.steps.is_empty() || self.steps.len() > 40 {
            bail!("plan must have 1–40 steps");
        }
        if self.ingredients.len() > 100 || self.notes.len() > 20 {
            bail!("too many ingredients or notes");
        }
        if self
            .ingredients
            .iter()
            .chain(&self.notes)
            .any(|s| s.len() > 2000)
        {
            bail!("ingredient/note too long");
        }
        for step in &self.steps {
            if step.instruction.trim().is_empty() || step.instruction.len() > 6000 {
                bail!("step text must be 1–6000 bytes");
            }
            if step.timers.len() > 4 {
                bail!(
                    "at most 4 simultaneous timers per step; split sequential waits into separate steps"
                );
            }
            for timer in &step.timers {
                if timer.label.trim().is_empty()
                    || timer.label.len() > 200
                    || !(1..=86_400).contains(&timer.seconds)
                {
                    bail!("timer requires a short label and 1–86400 seconds");
                }
            }
        }
        Ok(())
    }
}

static NUMBERED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:step\s+)?\d+[.):\-]?\s+(.+)$").unwrap());
static DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"(?i)\b(\d+(?:\.\d+)?|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve|half)(?:\s*(?:-|–|to)\s*(\d+(?:\.\d+)?))?\s*(seconds?|secs?|minutes?|mins?|hours?|hrs?)\b"
).unwrap()
});
fn quantity(s: &str) -> f64 {
    match s.to_ascii_lowercase().as_str() {
        "one" => 1.,
        "two" => 2.,
        "three" => 3.,
        "four" => 4.,
        "five" => 5.,
        "six" => 6.,
        "seven" => 7.,
        "eight" => 8.,
        "nine" => 9.,
        "ten" => 10.,
        "eleven" => 11.,
        "twelve" => 12.,
        "half" => 0.5,
        _ => s.parse().unwrap_or(0.),
    }
}
fn heading(s: &str) -> Option<bool> {
    let text = s
        .trim()
        .trim_matches(|c: char| c == ':' || c == '#' || c.is_whitespace())
        .to_ascii_lowercase();
    match text.as_str() {
        "instructions" | "directions" | "method" | "steps" | "preparation" => Some(true),
        "ingredients" | "ingredient list" => Some(false),
        _ => None,
    }
}

fn timers(text: &str) -> Vec<TimerSpec> {
    // Adjacent units ("1 hour and 20 minutes") form ONE duration. Other
    // multiple durations are ambiguous in fallback mode: leave them manual.
    if text.contains(['½', '¼', '¾', '/']) {
        return vec![];
    }
    let mut total = 0.;
    let mut end = None;
    let mut ambiguous = false;
    for capture in DURATION.captures_iter(text) {
        let span = capture.get(0).unwrap();
        if let Some(previous) = end {
            let between = text[previous..span.start()].trim().to_ascii_lowercase();
            if !matches!(between.as_str(), "" | "and") {
                ambiguous = true;
            }
        }
        let amount = quantity(&capture[1]); // range lower bound = a check, not guaranteed doneness
        let unit = capture[3].to_ascii_lowercase();
        total += amount
            * if unit.starts_with('h') {
                3600.
            } else if unit.starts_with('m') {
                60.
            } else {
                1.
            };
        end = Some(span.end());
    }
    if ambiguous || !(1. ..=86_400.).contains(&total) {
        return vec![];
    }
    vec![TimerSpec {
        label: "Recipe time — check progress when it rings".into(),
        seconds: total.round() as u64,
    }]
}

pub fn parse_demo(text: &str) -> Result<Plan> {
    if text.trim().is_empty() || text.len() > MAX_RECIPE_BYTES {
        bail!("paste a recipe of 1–32000 bytes");
    }
    let lines: Vec<_> = text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let first = lines.first().copied().unwrap_or("Your recipe");
    let title = if first.len() <= 120 && heading(first).is_none() && !NUMBERED.is_match(first) {
        first.trim_start_matches('#').trim().to_owned()
    } else {
        "Your recipe".into()
    };
    let mut ingredients = vec![];
    let mut raw_steps: Vec<String> = vec![];
    let has_method = lines.iter().any(|line| heading(line) == Some(true));
    let has_numbered = lines.iter().any(|line| NUMBERED.is_match(line));
    let mut in_method = false;
    let mut numbered_started = false;
    let mut in_ingredients = false;
    for (index, line) in lines.iter().enumerate() {
        if let Some(method) = heading(line) {
            in_method = method;
            in_ingredients = !method;
            continue;
        }
        if in_ingredients {
            ingredients.push(line.trim_start_matches(['-', '*', '•', ' ']).to_owned());
            continue;
        }
        if has_method && !in_method {
            continue;
        }
        if let Some(captures) = NUMBERED.captures(line) {
            numbered_started = true;
            raw_steps.push(captures[1].trim().to_owned());
        } else if in_method && line.starts_with(['-', '*', '•']) {
            raw_steps.push(line.trim_start_matches(['-', '*', '•', ' ']).to_owned());
        } else if !raw_steps.is_empty() && numbered_started {
            let previous = raw_steps.last_mut().unwrap();
            previous.push(' ');
            previous.push_str(line);
        } else if in_method || !(has_method || has_numbered || index == 0 && *line == title) {
            raw_steps.push((*line).to_owned());
        }
    }
    if raw_steps.is_empty() {
        bail!(
            "no instructions found; add an Instructions heading and numbered steps (or use live-model mode)"
        );
    }
    let steps = raw_steps
        .into_iter()
        .map(|instruction| RecipeStep {
            timers: timers(&instruction),
            instruction,
        })
        .collect();
    let plan = Plan { title, ingredients, steps, notes: vec![
        "No-key mode uses a simple text parser, not recipe understanding. Review the steps and detected timers against your original recipe. Use numbered instructions for best results.".into(),
        "Only explicit numeric/simple-word seconds, minutes or hours become timers. Ranges use the lower end as a progress check. Ambiguous multiple waits, fractions, overnight, preheating and doneness conditions need manual timing. Split sequential timed actions into separate numbered steps.".into(),
        "A timer is a reminder, not proof of doneness or food safety. Follow the original recipe and check food appropriately.".into(),
    ] };
    plan.validate()?;
    Ok(plan)
}
