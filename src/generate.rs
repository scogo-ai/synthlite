use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::task::JoinSet;

use crate::config::{self, KeyConfig, LoadMode};
use crate::error::{Error, Result};
use crate::preflight::{self, Origin, Seed};
use crate::provider::{self, Attempt};
use crate::store::{self, Appender, ParkedKey, State};
use crate::timefmt;

#[derive(Clone)]
pub struct GenerateOpts {
    /// Input file: JSONL (prompt records, Taskgen tasks or candidates) or a
    /// `.txt` file with one prompt per line.
    pub input: PathBuf,
    pub out: PathBuf,
    pub config: Option<PathBuf>,
    pub resume: bool,
    pub dry_run: bool,
    pub retry_failed: bool,
    /// `--retry-until-finish`: after a pass that leaves failures, run up to
    /// `UNTIL_FINISH_ROUNDS` more retry passes, then explain what still fails.
    pub retry_until_finish: bool,
    pub resume_parked_keys: bool,
    /// `--detailed`: decision-trace rows (see `docs/decision-traces.md`).
    /// `[generation].detailed` can also turn it on.
    pub detailed: bool,
    pub max_requests: Option<u64>,
    pub max_rows: Option<u64>,
    /// Heartbeat period for `progress` lines; `None` disables them.
    pub progress_interval: Option<Duration>,
}

/// Extra retry passes `--retry-until-finish` makes after the first pass.
pub const UNTIL_FINISH_ROUNDS: u32 = 3;

/// Pause before retry round `n` is `n * this`. `SYNTHLITE_RETRY_ROUND_PAUSE_SECS`
/// overrides it (tests set 0).
const ROUND_PAUSE_SECS: u64 = 30;

/// `generate::run` in a loop: fire and forget. Only exit 4 (some work items
/// failed) starts another round; a refusal, a stop, or an unexpected error
/// returns at once, because more rounds cannot fix those.
pub async fn run_until_finish(opts: GenerateOpts) -> Result<()> {
    if !opts.retry_until_finish || opts.dry_run {
        return run(opts).await;
    }
    let pause = std::env::var("SYNTHLITE_RETRY_ROUND_PAUSE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(ROUND_PAUSE_SECS);
    let mut round = 0u32;
    loop {
        let mut pass = opts.clone();
        pass.retry_failed = true;
        match run(pass).await {
            Err(Error::Failed(message)) => {
                if round >= UNTIL_FINISH_ROUNDS {
                    failure_summary(&opts.out);
                    return Err(Error::failed(format!(
                        "{message} after {UNTIL_FINISH_ROUNDS} retry rounds; fix the cause above, then rerun with --retry-failed"
                    )));
                }
                round += 1;
                let wait = pause * u64::from(round);
                eprintln!(
                    "retry_round {round}/{UNTIL_FINISH_ROUNDS} {message}; starting in {wait}s"
                );
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            other => return other,
        }
    }
}

/// Prints why work items are still failed: one line per (class, status, model)
/// with a count, a few task ids, and what to do. Reads `rows.errors.jsonl`
/// only; it never prints prompts or replies.
fn failure_summary(out: &Path) {
    let Ok(scan) = store::scan(out) else { return };
    let mut latest: std::collections::BTreeMap<String, Value> = Default::default();
    let Ok(lines) = store::complete_lines(&out.join("rows.errors.jsonl")) else {
        return;
    };
    for line in lines {
        if let Ok(value) = serde_json::from_slice::<Value>(line.trim_ascii_end()) {
            if let Some(id) = value.get("source_task_id").and_then(Value::as_str) {
                if scan.failed_attempts.contains_key(id) {
                    latest.insert(id.to_string(), value);
                }
            }
        }
    }
    let mut groups: std::collections::BTreeMap<(String, String, String), Vec<String>> =
        Default::default();
    for (id, record) in &latest {
        let text = |key: &str| {
            record
                .get(key)
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default()
        };
        groups
            .entry((text("error_class"), text("http_status"), text("model")))
            .or_default()
            .push(id.clone());
    }
    eprintln!(
        "failure_summary still_failed={} groups={}",
        latest.len(),
        groups.len()
    );
    for ((class, status, model), ids) in &groups {
        let sample: Vec<&str> = ids.iter().take(3).map(String::as_str).collect();
        eprintln!(
            "failure class={class} status={status} model={model} count={} examples={} next={}",
            ids.len(),
            sample.join(","),
            failure_hint(class)
        );
    }
}

fn failure_hint(class: &str) -> &'static str {
    match class {
        "truncated" => "raise [generation].max_output_tokens or use a model that stops sooner",
        "retries_exhausted" | "timeout" => {
            "check the provider is reachable, or lower max_concurrent"
        }
        "invalid_request" => "the provider rejects the request; check the model id and token field",
        "model_mismatch" => "set require_model_match = false or fix the model id",
        "invalid_trace" => "try another teacher for --detailed",
        "refusal" | "empty_assistant" => {
            "the teacher declined or returned nothing; try another model"
        }
        _ => "read rows.errors.jsonl",
    }
}

struct WorkItem {
    seed: Seed,
    /// Attempt number recorded in rows.errors.jsonl; keeps counting across runs.
    attempt: u32,
    /// Transient-failure tries in this run, compared against max_attempts.
    tries: u32,
    not_before: Instant,
    last_status: Option<u16>,
    /// Implicit budget this item was last sent with, after any step-down.
    output_token_budget: Option<u64>,
    /// Key whose provider rejected a larger budget for this item. A stepped
    /// budget only applies, and only teaches a ceiling, on that key.
    budget_key: Option<usize>,
}

struct KeyRt {
    cfg: KeyConfig,
    in_flight: usize,
    /// Adaptive concurrency: starts at `max_concurrent`, halves on a 429,
    /// and grows by one after `limit` clean responses in a row.
    limit: usize,
    clean_streak: usize,
    /// Bumped on every throttle. A 429 from a request admitted in an older
    /// epoch belongs to a burst already acted on and is not counted again.
    epoch: u64,
    /// Highest implicit output budget this key's provider accepted.
    budget_ceiling: u64,
    admissions: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
    headerless_429s: u32,
    parked: bool,
    park_reason: String,
    parked_at: String,
}

struct Finished {
    key_index: usize,
    request_id: u64,
    epoch: u64,
    item: WorkItem,
    kind: Attempt,
    usage: Option<provider::TokenUsage>,
    usage_expected: bool,
}

/// Streamed-token counters: one per in-flight request, plus the total of
/// requests that already finished. Per-request counts let the ETA subtract
/// work already done by replies that are still streaming.
#[derive(Default)]
struct LiveTokens {
    finished: u64,
    in_flight: std::collections::HashMap<u64, Arc<AtomicU64>>,
    next_id: u64,
}

impl LiveTokens {
    fn start(&mut self) -> (u64, Arc<AtomicU64>) {
        self.next_id += 1;
        let counter = Arc::new(AtomicU64::new(0));
        self.in_flight.insert(self.next_id, counter.clone());
        (self.next_id, counter)
    }

    fn finish(&mut self, request_id: u64) -> u64 {
        if let Some(counter) = self.in_flight.remove(&request_id) {
            let streamed = counter.load(Ordering::Relaxed);
            self.finished += streamed;
            return streamed;
        }
        0
    }

    fn streaming(&self) -> u64 {
        self.in_flight
            .values()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }

    fn total(&self) -> u64 {
        self.finished + self.streaming()
    }
}

pub async fn run(opts: GenerateOpts) -> Result<()> {
    let loaded = config::load(
        opts.config.as_deref(),
        LoadMode::Generate {
            detailed: opts.detailed,
        },
    )?;
    let preflight = preflight::load(&opts.input)?;
    if opts.out.exists() && !opts.out.is_dir() {
        return Err(Error::refuse(format!(
            "{} is not a directory",
            opts.out.display()
        )));
    }
    if opts.resume && !opts.out.exists() {
        return Err(Error::refuse(format!(
            "{} does not exist; --resume does not create a directory",
            opts.out.display()
        )));
    }
    let _lock = if opts.dry_run {
        None
    } else {
        if !opts.out.exists() {
            store::create_out_dir(&opts.out)?;
        }
        Some(store::lock_dir(&opts.out)?)
    };
    let is_run = store::is_synthlite_run(&opts.out);
    if opts.resume && !is_run {
        return Err(Error::refuse(format!(
            "{} is not a synthlite run; --resume requires an existing run",
            opts.out.display()
        )));
    }
    let resuming = is_run;
    let config_changed = if resuming {
        store::assert_resumable(
            &opts.out,
            &loaded.generator_config_hash,
            loaded.generation.get("detailed") == Some(&Value::Bool(true)),
        )?
    } else {
        false
    };

    let scan = if resuming {
        store::scan(&opts.out)?
    } else {
        store::Scan {
            committed: Default::default(),
            failed_attempts: Default::default(),
        }
    };
    let input_ids: HashSet<String> = preflight
        .seeds
        .iter()
        .map(|seed| seed.source_task_id.clone())
        .collect();
    let dropped = scan
        .committed
        .keys()
        .filter(|id| !input_ids.contains(*id))
        .count();
    let mut pending = VecDeque::new();
    let mut failed = 0usize;
    let mut complete = 0usize;
    for seed in &preflight.seeds {
        if scan.committed.contains_key(&seed.source_task_id) {
            complete += 1;
            continue;
        }
        if let Some(attempt) = scan.failed_attempts.get(&seed.source_task_id) {
            failed += 1;
            if opts.retry_failed {
                pending.push_back(WorkItem {
                    seed: seed.clone(),
                    attempt: attempt.saturating_add(1).max(1),
                    tries: 1,
                    not_before: Instant::now(),
                    last_status: None,
                    output_token_budget: None,
                    budget_key: None,
                });
            }
            continue;
        }
        pending.push_back(WorkItem {
            seed: seed.clone(),
            attempt: 1,
            tries: 1,
            not_before: Instant::now(),
            last_status: None,
            output_token_budget: None,
            budget_key: None,
        });
    }
    let mode = if resuming { "resume" } else { "start" };
    let pending_count = if opts.retry_failed {
        preflight.seeds.len() - complete
    } else {
        pending.len()
    };
    let line = format!(
        "model={} base={} out={} seeds={} {mode} pending={pending_count} complete={complete} failed={failed} dropped={dropped} version={}",
        loaded
            .keys
            .iter()
            .map(|key| key.model.as_str())
            .collect::<Vec<_>>()
            .join(","),
        loaded
            .keys
            .iter()
            .map(|key| key.base_url_display.as_str())
            .collect::<Vec<_>>()
            .join(","),
        opts.out.display(),
        preflight.seeds.len(),
        env!("CARGO_PKG_VERSION"),
    );
    if preflight.skipped_candidates > 0 {
        eprintln!(
            "preflight skipped {} Taskgen candidates with deterministic hard failures",
            preflight.skipped_candidates
        );
    }
    if opts.dry_run {
        eprintln!("{line}");
        return Ok(());
    }

    if resuming {
        store::repair_outputs(&opts.out)?;
    }

    let mut state = if let Some(mut existing) = store::read_state(&opts.out)? {
        existing.source_population_sha256 = preflight.population_sha256.clone();
        existing.taskgen_run_id = preflight.taskgen_run_id.clone();
        existing.seed_count = preflight.seeds.len() as u64;
        let changes = existing.adopt_config(
            &loaded.generator_config_hash,
            &loaded.generation,
            &timefmt::utc_now(),
        );
        if config_changed {
            eprintln!(
                "config_change resuming under a new [generation] config ({}); rows already written keep their own generator_config_hash",
                changes.join("; ")
            );
        }
        if opts.resume_parked_keys {
            existing.parked_keys.clear();
        }
        existing
    } else {
        State {
            schema_version: store::STATE_SCHEMA.into(),
            synthlite_version: env!("CARGO_PKG_VERSION").into(),
            generator_config_hash: loaded.generator_config_hash.clone(),
            generator_config: loaded.generation.clone(),
            source_population_sha256: preflight.population_sha256.clone(),
            taskgen_run_id: preflight.taskgen_run_id.clone(),
            seed_count: preflight.seeds.len() as u64,
            created_at: timefmt::utc_now(),
            generation_complete: false,
            parked_keys: Vec::new(),
            config_history: Vec::new(),
        }
    };

    eprintln!("{line}");
    if pending.is_empty() {
        state.generation_complete = input_ids.iter().all(|id| scan.committed.contains_key(id));
        store::write_state(&opts.out, &state)?;
        eprintln!("done requests=0 committed={complete} failed={failed} pending=0");
        return failed_result(failed);
    }

    state.generation_complete = false;
    store::write_state(&opts.out, &state)?;

    let mut keys: Vec<KeyRt> = loaded
        .keys
        .into_iter()
        .map(|cfg| {
            let parked = state.parked_keys.iter().find(|parked| parked.key == cfg.id);
            KeyRt {
                parked: parked.is_some(),
                park_reason: parked.map(|p| p.reason.clone()).unwrap_or_default(),
                parked_at: parked.map(|p| p.at.clone()).unwrap_or_default(),
                limit: cfg.max_concurrent,
                cfg,
                in_flight: 0,
                clean_streak: 0,
                epoch: 0,
                budget_ceiling: config::DEFAULT_MAX_OUTPUT_TOKENS,
                admissions: VecDeque::new(),
                cooldown_until: None,
                headerless_429s: 0,
            }
        })
        .collect();

    let client = provider::http_client()
        .map_err(|err| Error::Unexpected(anyhow::anyhow!("http client: {err}")))?;
    let mut rows = None;
    let mut errors = None;
    let generation = loaded.generation;
    let hash = loaded.generator_config_hash;
    let mut requests = 0u64;
    let mut committed_now = 0u64;
    let mut inflight: JoinSet<std::result::Result<Finished, Error>> = JoinSet::new();
    let mut class_counts: BTreeCount = BTreeCount::default();
    let started = Instant::now();
    let seed_total = input_ids.len();
    let committed_before = scan.committed.len();
    let mut next_beat = opts.progress_interval.map(|every| started + every);
    let mut live = LiveTokens::default();
    let mut last_beat = (started, 0u64);

    loop {
        if next_beat.is_some_and(|beat| beat <= Instant::now()) {
            let now = Instant::now();
            class_counts.streaming_tokens = live.streaming();
            let window = now.saturating_duration_since(last_beat.0).as_secs_f64();
            class_counts.stream_piece_rate = if window > 0.0 {
                live.total().saturating_sub(last_beat.1) as f64 / window
            } else {
                0.0
            };
            last_beat = (now, live.total());
            class_counts.tok_s = observed_token_rate(
                class_counts.output_tokens,
                now.saturating_duration_since(started),
            );
            eprintln!(
                "{}",
                progress_line(
                    &class_counts,
                    committed_before,
                    seed_total,
                    pending.len(),
                    inflight.len(),
                    requests,
                    started.elapsed(),
                )
            );
            next_beat = opts.progress_interval.map(|every| Instant::now() + every);
        }
        admit(
            &mut pending,
            &mut keys,
            &mut inflight,
            &client,
            &generation,
            &opts,
            &mut requests,
            committed_now,
            &mut live,
        )?;
        if inflight.is_empty() {
            if pending.is_empty() {
                break;
            }
            if keys.iter().all(|key| key.parked) {
                sync_parked(&mut state, &keys, &opts.out)?;
                return Err(Error::stopped("all keys are parked; pending work remains"));
            }
            if cap_reached(&opts, requests, committed_now) {
                sync_parked(&mut state, &keys, &opts.out)?;
                return Err(Error::stopped(
                    "stopped by --max-requests or --max-rows; pending work remains",
                ));
            }
            if let Some(wait) = next_wait(&keys, &pending, Instant::now()) {
                tokio::time::sleep(earliest(Some(wait), until(next_beat)).unwrap_or(wait)).await;
                continue;
            }
            return Err(Error::stopped("no key can admit pending work"));
        }
        let wait = earliest(next_wait(&keys, &pending, Instant::now()), until(next_beat));
        let finished = tokio::select! {
            biased;
            joined = inflight.join_next() => {
                match joined {
                    Some(Ok(Ok(finished))) => finished,
                    Some(Ok(Err(err))) => return Err(err),
                    Some(Err(err)) => {
                        return Err(Error::Unexpected(anyhow::anyhow!("worker failed: {err}")));
                    }
                    None => return Err(Error::Unexpected(anyhow::anyhow!("worker set empty"))),
                }
            }
            _ = sleep_until(wait), if wait.is_some() => continue,
        };
        if let Some(key) = keys.get_mut(finished.key_index) {
            key.in_flight = key.in_flight.saturating_sub(1);
        }
        let finished_live_tokens = live.finish(finished.request_id);
        if let Some(usage) = &finished.usage {
            class_counts.usage_reported += 1;
            class_counts.input_tokens += u64::try_from(usage.prompt_tokens).unwrap_or(0);
            class_counts.output_tokens += u64::try_from(usage.completion_tokens).unwrap_or(0);
        } else if finished.usage_expected {
            class_counts.usage_missing += 1;
        }
        let answered = match &finished.kind {
            Attempt::Success(_) => Some(200),
            Attempt::Permanent { http_status, .. } | Attempt::Retry { http_status, .. } => {
                *http_status
            }
            Attempt::Unauthorized { http_status } => Some(*http_status),
            Attempt::BudgetRejected => Some(400),
            Attempt::RateLimit { .. } => None,
        };
        if let Some(status) = answered {
            let key = &mut keys[finished.key_index];
            key.clean_streak += 1;
            if key.clean_streak >= key.limit && key.limit < key.cfg.max_concurrent {
                eprintln!(
                    "throttle {} max_concurrent={}->{}",
                    key.cfg.display_id,
                    key.limit,
                    key.limit + 1
                );
                key.limit += 1;
                key.clean_streak = 0;
            }
            // A 2xx at a stepped-down budget shows what this provider accepts.
            if (200..300).contains(&status) {
                if let (Some(budget), Some(stepped_on)) =
                    (finished.item.output_token_budget, finished.item.budget_key)
                {
                    if stepped_on == finished.key_index && budget < key.budget_ceiling {
                        eprintln!(
                            "budget {} max_output_tokens={}->{budget}",
                            key.cfg.display_id, key.budget_ceiling
                        );
                        key.budget_ceiling = budget;
                    }
                }
            }
        }
        match finished.kind {
            Attempt::Success(success) => {
                if let Some(key) = keys.get_mut(finished.key_index) {
                    key.headerless_429s = 0;
                }
                let key = &keys[finished.key_index].cfg;
                let line = encode_row(&finished.item, key, &hash, &generation, &success)?;
                append(&mut rows, &opts.out.join("rows.jsonl"), &line)?;
                committed_now += 1;
                class_counts.success += 1;
                class_counts.completed_live_tokens += finished_live_tokens;
            }
            Attempt::Permanent { class, http_status } => {
                if let Some(key) = keys.get_mut(finished.key_index) {
                    key.headerless_429s = 0;
                }
                let key = &keys[finished.key_index].cfg;
                let line =
                    encode_error(&finished.item, key, &hash, &generation, class, http_status)?;
                append(&mut errors, &opts.out.join("rows.errors.jsonl"), &line)?;
                report_failure(&finished.item, &generation, class, http_status);
                class_counts.failed += 1;
                class_counts.bump(class);
            }
            Attempt::Retry {
                http_status,
                reason,
            } => {
                if http_status.is_some() {
                    if let Some(key) = keys.get_mut(finished.key_index) {
                        key.headerless_429s = 0;
                    }
                }
                class_counts.retry += 1;
                let max_attempts = keys[finished.key_index].cfg.max_attempts;
                let mut item = finished.item;
                item.last_status = http_status;
                if item.tries >= max_attempts {
                    let key = &keys[finished.key_index].cfg;
                    let line = encode_error(
                        &item,
                        key,
                        &hash,
                        &generation,
                        "retries_exhausted",
                        item.last_status,
                    )?;
                    append(&mut errors, &opts.out.join("rows.errors.jsonl"), &line)?;
                    report_failure(&item, &generation, "retries_exhausted", item.last_status);
                    class_counts.failed += 1;
                    class_counts.bump("retries_exhausted");
                } else {
                    let failed_try = item.tries;
                    let delay = backoff(failed_try);
                    eprintln!(
                        "retry {} try={failed_try}/{max_attempts} reason={reason} backoff={}",
                        short_id(&item.seed.source_task_id),
                        format_duration(delay)
                    );
                    item.tries += 1;
                    item.attempt += 1;
                    item.not_before = Instant::now() + delay;
                    pending.push_back(item);
                }
            }
            Attempt::BudgetRejected => {
                let current = provider::resolve_max_output_tokens(
                    &generation,
                    finished.item.output_token_budget,
                );
                if let Some(next) = provider::next_lower_budget(current) {
                    eprintln!(
                        "budget {} max_output_tokens={current}->{next} reason=http_400",
                        short_id(&finished.item.seed.source_task_id)
                    );
                    class_counts.budget_retry += 1;
                    let mut item = finished.item;
                    item.output_token_budget = Some(next);
                    item.budget_key = Some(finished.key_index);
                    pending.push_front(item);
                } else {
                    let key = &keys[finished.key_index].cfg;
                    let line = encode_error(
                        &finished.item,
                        key,
                        &hash,
                        &generation,
                        "invalid_request",
                        Some(400),
                    )?;
                    append(&mut errors, &opts.out.join("rows.errors.jsonl"), &line)?;
                    report_failure(&finished.item, &generation, "invalid_request", Some(400));
                    class_counts.failed += 1;
                    class_counts.bump("invalid_request");
                }
            }
            Attempt::RateLimit { retry_after } => {
                class_counts.rate_limit += 1;
                let mut park = false;
                {
                    let key = &mut keys[finished.key_index];
                    if finished.epoch != key.epoch {
                        // Part of a burst this key already throttled for.
                        pending.push_back(finished.item);
                        continue;
                    }
                    key.epoch += 1;
                    key.clean_streak = 0;
                    let halved = (key.limit / 2).max(1);
                    if halved < key.limit {
                        eprintln!(
                            "throttle {} max_concurrent={}->{halved} reason=429",
                            key.cfg.display_id, key.limit
                        );
                        key.limit = halved;
                    }
                    if let Some(delay) = retry_after {
                        key.headerless_429s = 0;
                        key.cooldown_until = Some(Instant::now() + delay);
                    } else {
                        key.headerless_429s += 1;
                        key.cooldown_until = Some(
                            Instant::now()
                                + Duration::from_secs(key.cfg.rate_limit_cooldown_seconds),
                        );
                        if key.headerless_429s >= key.cfg.headerless_429_limit {
                            key.parked = true;
                            key.park_reason = "headerless_429".into();
                            key.parked_at = timefmt::utc_now();
                            park = true;
                        }
                    }
                }
                if park {
                    eprintln!(
                        "parked {} headerless_429",
                        keys[finished.key_index].cfg.display_id
                    );
                    sync_parked(&mut state, &keys, &opts.out)?;
                }
                pending.push_back(finished.item);
            }
            Attempt::Unauthorized { .. } => {
                keys[finished.key_index].parked = true;
                keys[finished.key_index].park_reason = "unauthorized".into();
                keys[finished.key_index].parked_at = timefmt::utc_now();
                eprintln!(
                    "parked {} unauthorized",
                    keys[finished.key_index].cfg.display_id
                );
                sync_parked(&mut state, &keys, &opts.out)?;
                pending.push_back(finished.item);
            }
        }
    }

    let final_scan = store::scan(&opts.out)?;
    state.generation_complete = input_ids
        .iter()
        .all(|id| final_scan.committed.contains_key(id));
    sync_parked(&mut state, &keys, &opts.out)?;
    let failed_left = input_ids
        .iter()
        .filter(|id| final_scan.failed_attempts.contains_key(*id))
        .count();
    eprintln!(
        "done requests={requests} committed={} failed={failed_left} pending={} success={} rate_limit={} retry={} budget_retry={} input_tokens={} output_tokens={} usage_missing={} elapsed={}",
        final_scan.committed.len(),
        input_ids.len().saturating_sub(final_scan.committed.len() + failed_left),
        class_counts.success,
        class_counts.rate_limit,
        class_counts.retry,
        class_counts.budget_retry,
        class_counts.input_tokens,
        class_counts.output_tokens,
        class_counts.usage_missing,
        format_duration(started.elapsed()),
    );
    failed_result(failed_left)
}

fn failed_result(failed: usize) -> Result<()> {
    if failed > 0 {
        return Err(Error::failed(format!(
            "{failed} work items failed; rerun with --retry-failed"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn admit(
    pending: &mut VecDeque<WorkItem>,
    keys: &mut [KeyRt],
    inflight: &mut JoinSet<std::result::Result<Finished, Error>>,
    client: &reqwest::Client,
    generation: &Value,
    opts: &GenerateOpts,
    requests: &mut u64,
    committed_now: u64,
    live: &mut LiveTokens,
) -> Result<()> {
    loop {
        if cap_reached(opts, *requests, committed_now) || pending.is_empty() {
            break;
        }
        let now = Instant::now();
        let Some(pos) = pending.iter().position(|item| item.not_before <= now) else {
            break;
        };
        let Some(key_index) = (0..keys.len()).find(|index| keys[*index].can_admit(now)) else {
            break;
        };
        let Some(item) = pending.remove(pos) else {
            break;
        };
        let mut item = item;
        let key = &mut keys[key_index];
        key.in_flight += 1;
        key.note_admission(now);
        *requests += 1;
        if !provider::max_output_tokens_explicit(generation) {
            let wanted = match item.budget_key {
                Some(stepped_on) if stepped_on == key_index => item
                    .output_token_budget
                    .unwrap_or(config::DEFAULT_MAX_OUTPUT_TOKENS),
                _ => config::DEFAULT_MAX_OUTPUT_TOKENS,
            };
            item.output_token_budget = Some(wanted.min(key.budget_ceiling));
        }
        let epoch = key.epoch;
        let cfg = key.cfg.clone();
        let generation = generation.clone();
        let client = client.clone();
        let budget_override = item.output_token_budget;
        let (request_id, live_tokens) = live.start();
        inflight.spawn(async move {
            let result = provider::chat(
                &client,
                &cfg,
                &generation,
                &item.seed.prompt,
                budget_override,
                &live_tokens,
            )
            .await;
            Ok(Finished {
                key_index,
                request_id,
                epoch,
                item,
                kind: result.attempt,
                usage: result.usage,
                usage_expected: result.usage_expected,
            })
        });
    }
    Ok(())
}

impl KeyRt {
    fn can_admit(&mut self, now: Instant) -> bool {
        if self.parked || self.in_flight >= self.limit {
            return false;
        }
        if self.cooldown_until.is_some_and(|until| until > now) {
            return false;
        }
        self.rpm_ok(now)
    }

    fn rpm_ok(&mut self, now: Instant) -> bool {
        let Some(limit) = self.cfg.requests_per_minute else {
            return true;
        };
        let window = Duration::from_secs(60);
        while self
            .admissions
            .front()
            .is_some_and(|stamp| now.saturating_duration_since(*stamp) >= window)
        {
            self.admissions.pop_front();
        }
        self.admissions.len() < limit as usize
    }

    fn note_admission(&mut self, now: Instant) {
        if self.cfg.requests_per_minute.is_some() {
            self.admissions.push_back(now);
        }
    }
}

fn cap_reached(opts: &GenerateOpts, requests: u64, committed_now: u64) -> bool {
    if opts.max_requests.is_some_and(|limit| requests >= limit) {
        return true;
    }
    opts.max_rows.is_some_and(|limit| committed_now >= limit)
}

fn next_wait(keys: &[KeyRt], pending: &VecDeque<WorkItem>, now: Instant) -> Option<Duration> {
    let mut soonest: Option<Instant> = None;
    let consider = |soonest: &mut Option<Instant>, instant: Instant| {
        if instant > now {
            *soonest = Some(
                soonest
                    .map(|current| current.min(instant))
                    .unwrap_or(instant),
            );
        }
    };
    for key in keys {
        if key.parked {
            continue;
        }
        if let Some(until) = key.cooldown_until {
            consider(&mut soonest, until);
        }
        if let Some(limit) = key.cfg.requests_per_minute {
            if key.admissions.len() >= limit as usize {
                if let Some(oldest) = key.admissions.front() {
                    consider(&mut soonest, *oldest + Duration::from_secs(60));
                }
            }
        }
    }
    for item in pending {
        consider(&mut soonest, item.not_before);
    }
    soonest.map(|instant| instant.saturating_duration_since(now))
}

/// Seed ids are `task_<sha256>`; the first 12 hex digits are enough to grep
/// rows.jsonl and rows.errors.jsonl.
fn short_id(source_task_id: &str) -> &str {
    let end = source_task_id
        .char_indices()
        .nth("task_".len() + 12)
        .map_or(source_task_id.len(), |(index, _)| index);
    &source_task_id[..end]
}

fn report_failure(item: &WorkItem, generation: &Value, class: &str, http_status: Option<u16>) {
    let status = http_status.map_or_else(|| "none".to_string(), |status| status.to_string());
    eprintln!(
        "failed {} class={class} status={status} max_output_tokens={}",
        short_id(&item.seed.source_task_id),
        provider::resolve_max_output_tokens(generation, item.output_token_budget)
    );
}

fn progress_line(
    counts: &BTreeCount,
    committed_before: usize,
    seed_total: usize,
    queued: usize,
    in_flight: usize,
    requests: u64,
    elapsed: Duration,
) -> String {
    let committed = committed_before as u64 + counts.success;
    let remaining = eta(counts, queued as u64, in_flight as u64)
        .map(format_duration)
        .unwrap_or_else(|| "?".into());
    let rate = if counts.usage_reported > 0 {
        format!("{:.1}", counts.tok_s)
    } else {
        "?".into()
    };
    let mut line = format!(
        "{} progress requests={requests} committed={committed}/{seed_total} in_flight={in_flight} queued={queued} failed={} retry={}",
        timefmt::local_progress_now(),
        counts.failed,
        counts.retry,
    );
    if counts.budget_retry > 0 {
        line.push_str(&format!(" budget_retry={}", counts.budget_retry));
    }
    if counts.rate_limit > 0 {
        line.push_str(&format!(" rate_limit={}", counts.rate_limit));
    }
    line.push_str(&format!(
        " tok_s={rate} input_tokens={} output_tokens={} elapsed={}",
        counts.input_tokens,
        counts.output_tokens,
        format_duration(elapsed),
    ));
    if counts.usage_missing > 0 {
        line.push_str(&format!(" usage_missing={}", counts.usage_missing));
    }
    line.push_str(&format!(" finish_in={remaining}"));
    line
}

/// Approximate remaining work in streamed chunks, using the average completed
/// reply and the latest aggregate streaming rate. The final in-flight replies
/// can run longer than the average, so their ETA is less reliable.
fn eta(counts: &BTreeCount, queued: u64, in_flight: u64) -> Option<Duration> {
    if counts.success == 0
        || counts.stream_piece_rate <= 0.0
        || !counts.stream_piece_rate.is_finite()
    {
        return None;
    }
    let average = if counts.completed_live_tokens > 0 {
        counts.completed_live_tokens as f64 / counts.success as f64
    } else {
        counts.output_tokens as f64 / counts.success as f64
    };
    let in_flight_left = (average * in_flight as f64 - counts.streaming_tokens as f64).max(0.0);
    let needed = average * queued as f64 + in_flight_left;
    if needed <= 0.0 && queued + in_flight > 0 {
        return None;
    }
    Some(Duration::from_secs_f64(needed / counts.stream_piece_rate))
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn observed_token_rate(output_tokens: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds > 0.0 {
        output_tokens as f64 / seconds
    } else {
        0.0
    }
}

fn until(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

fn earliest(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

async fn sleep_until(wait: Option<Duration>) {
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
}

fn sync_parked(state: &mut State, keys: &[KeyRt], out: &Path) -> Result<()> {
    state.parked_keys = keys
        .iter()
        .filter(|key| key.parked)
        .map(|key| ParkedKey {
            key: key.cfg.id.clone(),
            reason: key.park_reason.clone(),
            at: key.parked_at.clone(),
        })
        .collect();
    store::write_state(out, state)
}

fn append(slot: &mut Option<Appender>, path: &Path, line: &[u8]) -> Result<()> {
    if slot.is_none() {
        *slot = Some(Appender::open(path)?);
    }
    slot.as_mut()
        .ok_or_else(|| Error::Unexpected(anyhow::anyhow!("appender missing")))?
        .append_line(line)
}

fn encode_row(
    item: &WorkItem,
    key: &KeyConfig,
    hash: &str,
    generation: &Value,
    success: &provider::ChatSuccess,
) -> Result<Vec<u8>> {
    let mut messages = Vec::new();
    if let Some(system) = generation
        .get("system_message")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": item.seed.prompt}));
    messages.push(json!({"role": "assistant", "content": success.text}));
    let usage = match &success.usage {
        Some(usage) => json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
        }),
        None => Value::Null,
    };
    // `model` is the model requested; `served_model` is the one the provider
    // says answered, which differs behind a router.
    let mut metadata = json!({
        "category": item.seed.category,
        "domain": item.seed.domain,
        "subdomain": item.seed.subdomain,
        "difficulty": item.seed.difficulty,
        "coordinates": item.seed.coordinates,
        "language": item.seed.language,
        "taskgen_model": item.seed.taskgen_model,
        "provider": key.provider,
        "base_url": key.base_url,
        "model": key.model,
        "served_model": success.served_model,
        "generated_at": timefmt::utc_now(),
        "max_output_tokens": provider::resolve_max_output_tokens(generation, item.output_token_budget),
        "usage": usage,
    });
    if let Origin::Prompt {
        id,
        metadata: input,
    } = &item.seed.origin
    {
        metadata["input_id"] = json!(id);
        metadata["input_metadata"] = input.clone().unwrap_or(Value::Null);
    }
    let row = json!({
        "schema_version": "synthlite.sft.v1",
        "source_task_id": item.seed.source_task_id,
        "variant_index": 0,
        "generator_config_hash": hash,
        "messages": messages,
        "metadata": metadata,
    });
    let mut bytes = serde_json::to_vec(&row)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn encode_error(
    item: &WorkItem,
    key: &KeyConfig,
    hash: &str,
    generation: &Value,
    class: &str,
    http_status: Option<u16>,
) -> Result<Vec<u8>> {
    let record = json!({
        "source_task_id": item.seed.source_task_id,
        "variant_index": 0,
        "generator_config_hash": hash,
        "provider": key.provider,
        "model": key.model,
        "attempt": item.attempt,
        "error_class": class,
        "http_status": http_status,
        "max_output_tokens": provider::resolve_max_output_tokens(generation, item.output_token_budget),
        "at": timefmt::utc_now(),
    });
    let mut bytes = serde_json::to_vec(&record)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn backoff(failed_attempt: u32) -> Duration {
    let shift = failed_attempt.saturating_sub(1).min(6);
    let cap = (1u64 << shift).min(60);
    jitter(cap)
}

static JITTER: AtomicU64 = AtomicU64::new(0x1234_5678);

fn jitter(cap: u64) -> Duration {
    if cap == 0 {
        return Duration::from_millis(0);
    }
    let tick = JITTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(1);
    let mixed = tick
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(nanos | 1);
    Duration::from_secs(mixed % (cap + 1))
}

#[derive(Default)]
struct BTreeCount {
    success: u64,
    retry: u64,
    rate_limit: u64,
    budget_retry: u64,
    failed: u64,
    input_tokens: u64,
    output_tokens: u64,
    usage_missing: u64,
    usage_reported: u64,
    /// Streamed pieces from successful replies, calibrated against the internal piece rate.
    completed_live_tokens: u64,
    /// Provider-reported output tokens over elapsed run time.
    tok_s: f64,
    /// Internal streamed pieces per second since the previous heartbeat.
    stream_piece_rate: f64,
    /// Tokens streamed so far by requests still in flight.
    streaming_tokens: u64,
    classes: std::collections::BTreeMap<String, u64>,
}

impl BTreeCount {
    fn bump(&mut self, class: &str) {
        *self.classes.entry(class.to_string()).or_insert(0) += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_hours_minutes_seconds() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
        assert_eq!(format_duration(Duration::from_secs(724)), "12m04s");
        assert_eq!(format_duration(Duration::from_secs(4505)), "1h15m05s");
    }

    #[test]
    fn token_rate_uses_reported_output_tokens() {
        assert_eq!(observed_token_rate(1_000, Duration::from_secs(5)), 200.0);
    }

    #[test]
    fn short_id_keeps_twelve_hex_digits() {
        assert_eq!(
            short_id("task_cc3e0bec7b87ec223ae0ef01"),
            "task_cc3e0bec7b87"
        );
        assert_eq!(short_id("task_abc"), "task_abc");
    }

    #[test]
    fn progress_line_has_local_timestamp_and_eta_in_requested_order() {
        let mut counts = BTreeCount {
            stream_piece_rate: 70.0,
            ..BTreeCount::default()
        };
        let line = progress_line(&counts, 0, 10, 6, 4, 4, Duration::from_secs(60));
        let (stamp, fields) = line.split_once(' ').unwrap();
        assert!(
            chrono::NaiveDateTime::parse_from_str(stamp, "%d-%m-%y:%H:%M:%S").is_ok(),
            "{line}"
        );
        assert!(
            fields.starts_with("progress requests=4 committed=0/10 in_flight=4 queued=6 failed=0"),
            "{line}"
        );
        assert!(!line.contains("budget_retry="), "{line}");
        assert!(!line.contains("rate_limit="), "{line}");
        assert!(line.ends_with("finish_in=?"), "{line}");
        counts.success = 2;
        counts.output_tokens = 10_000;
        counts.streaming_tokens = 5_000;
        counts.stream_piece_rate = 80.0;
        // 1 queued x 5000 + (4 x 5000 - 5000) in flight = 20000 at 80 tok/s.
        let line = progress_line(&counts, 3, 10, 1, 4, 9, Duration::from_secs(600));
        assert!(line.contains("committed=5/10"), "{line}");
        assert!(line.ends_with("elapsed=10m00s finish_in=4m10s"), "{line}");
    }

    #[test]
    fn eta_remains_visible_for_final_in_flight_replies() {
        let counts = BTreeCount {
            success: 2,
            output_tokens: 10_000,
            completed_live_tokens: 2_000,
            streaming_tokens: 500,
            stream_piece_rate: 50.0,
            ..BTreeCount::default()
        };
        assert_eq!(eta(&counts, 0, 1), Some(Duration::from_secs(10)));
        let line = progress_line(&counts, 2, 3, 0, 1, 3, Duration::from_secs(600));
        assert!(line.ends_with("finish_in=10s"), "{line}");
    }

    #[test]
    fn eta_for_a_large_run_is_dominated_by_queued_work() {
        let counts = BTreeCount {
            success: 100,
            output_tokens: 800_000,
            streaming_tokens: 40_000,
            stream_piece_rate: 100.0,
            ..BTreeCount::default()
        };
        // 1000 queued x 8000 + (8 x 8000 - 40000) = 8_024_000 tokens.
        assert_eq!(eta(&counts, 1000, 8), Some(Duration::from_secs(80_240)));
    }
}
