(() => {
  "use strict";

  const ui = {
    tabs: document.querySelector("#repo-tabs"),
    dashboard: document.querySelector("#dashboard"),
    overview: document.querySelector("#repo-overview"),
    caravans: document.querySelector("#caravans"),
    concatControl: document.querySelector("#concat-control"),
    concatSource: document.querySelector("#concat-source"),
    concatTarget: document.querySelector("#concat-target"),
    concatActor: document.querySelector("#concat-actor"),
    concatReason: document.querySelector("#concat-reason"),
    planConcat: document.querySelector("#plan-concat"),
    executeConcat: document.querySelector("#execute-concat"),
    concatPlanHash: document.querySelector("#concat-plan-hash"),
    saloon: document.querySelector("#saloon"),
    saloonCount: document.querySelector("#saloon-count"),
    decisions: document.querySelector("#decisions"),
    attentionCount: document.querySelector("#attention-count"),
    workspace: document.querySelector("#workspace"),
    repositorySidebar: document.querySelector("#repository-sidebar"),
    attentionSidebar: document.querySelector("#attention-sidebar"),
    toggleRepositories: document.querySelector("#toggle-repositories"),
    toggleAttention: document.querySelector("#toggle-attention"),
    connection: document.querySelector("#connection"),
    activity: document.querySelector("#activity"),
    refresh: document.querySelector("#refresh-all"),
    plan: document.querySelector("#plan-sync"),
    sync: document.querySelector("#sync-all"),
    evidence: document.querySelector("#show-evidence"),
    config: document.querySelector("#show-config"),
    inspector: document.querySelector("#inspector"),
    inspectorTitle: document.querySelector("#inspector-title"),
    inspectorKicker: document.querySelector("#inspector-kicker"),
    inspectorContent: document.querySelector("#inspector-content"),
    closeInspector: document.querySelector("#close-inspector"),
    toasts: document.querySelector("#toast-region"),
    empty: document.querySelector("#empty-template"),
  };

  let state = null;
  let selectedRepo = null;
  let requestInFlight = false;
  let inspectorMode = null;
  const concatPlans = new Map();
  const sidebarState = {
    repositories: window.localStorage.getItem("caravan.sidebar.repositories") !== "collapsed",
    attention: window.localStorage.getItem("caravan.sidebar.attention") !== "collapsed",
  };
  if (window.matchMedia("(max-width: 900px)").matches) {
    if (!window.localStorage.getItem("caravan.sidebar.repositories")) sidebarState.repositories = false;
    if (!window.localStorage.getItem("caravan.sidebar.attention")) sidebarState.attention = false;
  }
  const SALOON_ORDER = ["ready", "conflicting", "saddling", "other", "bounty"];
  const SALOON_META = {
    ready: ["Ready", "Mechanically clean against at least one exact current destination"],
    conflicting: ["Conflicting", "No exact current destination merges cleanly"],
    saddling: ["Saddling Up", "Known work or provider state is still incomplete"],
    other: ["Other", "Exact target compatibility is unknown or still being checked"],
    bounty: ["Bounty List", "Skipped or evicted and not yet fixed"],
  };

  const escapeHtml = (value) => String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
  const shortOid = (value) => value ? String(value).slice(0, 9) : "unknown";
  const labelNames = (pr) => Array.from(pr?.labels ?? []);
  const hasLabel = (pr, name) => labelNames(pr).includes(name);
  const prValues = (status) => Object.values(status?.analysis?.pull_requests ?? {});
  const exactPr = (status, number) => status?.analysis?.pull_requests?.[String(number)] ?? null;
  const openPr = (pr) => String(pr?.state ?? "").toLowerCase() === "open";
  const normalized = (value) => String(value ?? "unknown").toLowerCase();
  const selected = () => state?.repositories?.find((item) => item.id === selectedRepo) ?? null;

  function applySidebarState() {
    ui.workspace.classList.toggle("repositories-collapsed", !sidebarState.repositories);
    ui.workspace.classList.toggle("attention-collapsed", !sidebarState.attention);
    ui.repositorySidebar.hidden = !sidebarState.repositories;
    ui.attentionSidebar.hidden = !sidebarState.attention;
    ui.toggleRepositories.setAttribute("aria-expanded", String(sidebarState.repositories));
    ui.toggleAttention.setAttribute("aria-expanded", String(sidebarState.attention));
  }

  function toggleSidebar(name, force) {
    sidebarState[name] = force ?? !sidebarState[name];
    if (sidebarState[name] && window.matchMedia("(max-width: 900px)").matches) {
      const other = name === "repositories" ? "attention" : "repositories";
      sidebarState[other] = false;
      window.localStorage.setItem(`caravan.sidebar.${other}`, "collapsed");
    }
    window.localStorage.setItem(`caravan.sidebar.${name}`, sidebarState[name] ? "expanded" : "collapsed");
    applySidebarState();
  }

  function empty(message) {
    const node = ui.empty.content.firstElementChild.cloneNode(true);
    node.querySelector("p").textContent = message;
    return node.outerHTML;
  }

  function badge(text, tone = "") {
    return `<span class="badge ${tone}">${escapeHtml(text)}</span>`;
  }

  function setBusy(busy) {
    const actionBusy = selected()?.actions?.some((job) => ["queued", "running"].includes(job.state)) ?? false;
    ui.activity.hidden = !(busy || actionBusy);
    ui.refresh.disabled = busy || actionBusy;
    ui.connection.setAttribute("aria-busy", String(busy || actionBusy));
  }

  function repoName(repo) {
    const statusRepo = repo.status?.repository;
    if (statusRepo) return `${statusRepo.owner}/${statusRepo.name}`;
    return repo.path.split("/").filter(Boolean).at(-1) || repo.id;
  }

  function prUrl(pr) {
    if (pr?.url?.startsWith("https://")) return pr.url;
    const repo = selected()?.status?.repository;
    return repo ? `https://github.com/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}/pull/${pr.number}` : "#";
  }

  function prAnchor(pr, label, className = "") {
    return `<a class="${escapeHtml(className)}" href="${escapeHtml(prUrl(pr))}" target="_blank" rel="noopener noreferrer">${escapeHtml(label)}</a>`;
  }

  // bd-eff1dc: a rollup keeps every historical run of a required check on the
  // same head. Discovery marks the ones a later run superseded, so summaries
  // must judge only current rows or a long-finished cancelled run reports the
  // PR as blocked forever.
  function currentChecks(checks) {
    return (checks ?? []).filter((check) => !check.superseded);
  }

  function checkTone(checks) {
    const current = currentChecks(checks);
    if (!current.length) return ["No checks", "warn"];
    const states = current.map((check) => normalized(check.state));
    if (states.some((item) => ["failure", "error", "cancelled", "timed_out", "unknown"].includes(item))) return ["CI blocked", "bad"];
    if (states.some((item) => ["queued", "pending", "in_progress", "expected"].includes(item))) return ["CI running", "warn"];
    return ["CI green", "good"];
  }

  function checkRows(checks, showPassing = false) {
    // History stays visible, but clearly marked, so a reader can see that the
    // red row above the green one has already been rerun.
    const rows = (checks ?? []).filter((check) => showPassing || check.superseded || normalized(check.state) !== "success");
    if (!rows.length) return "";
    return `<div class="check-list">${rows.map((check) => {
      const stateName = normalized(check.state);
      const tone = check.superseded ? "" : stateName === "success" ? "good" : ["queued", "pending", "in_progress", "expected"].includes(stateName) ? "warn" : "bad";
      const name = check.details_url
        ? `<a href="${escapeHtml(check.details_url)}" target="_blank" rel="noopener noreferrer">${escapeHtml(check.name)}</a>`
        : `<span>${escapeHtml(check.name)}</span>`;
      const label = check.superseded ? `${check.provider_state || stateName} · superseded` : check.provider_state || stateName;
      return `<div class="check-row${check.superseded ? " superseded" : ""}">${name}${badge(label, tone)}</div>`;
    }).join("")}</div>`;
  }

  function renderTabs() {
    ui.tabs.innerHTML = state.repositories.map((repo) => {
      const isSelected = repo.id === selectedRepo;
      const health = repo.error ? "bad" : repo.status?.healthy ? "good" : "";
      const detail = repo.refreshing ? "Refreshing" : repo.error ? repo.error.code : repo.status?.healthy ? "Healthy" : "Needs attention";
      return `<button class="repo-tab" role="tab" aria-selected="${isSelected}" data-repo="${escapeHtml(repo.id)}">
        <strong>${escapeHtml(repoName(repo))}</strong>
        <small><span class="status-dot ${health}"></span>${escapeHtml(detail)}</small>
      </button>`;
    }).join("");
  }

  function renderOverview(repo) {
    if (repo.error && !repo.status) {
      ui.overview.innerHTML = `<div class="error-banner"><strong>${escapeHtml(repo.error.code)}</strong><br>${escapeHtml(repo.error.message)}</div>`;
      return;
    }
    const status = repo.status;
    const caravans = status?.analysis?.fleet?.caravans ?? [];
    const activeCaravans = caravans.filter((caravan) => !caravan.parked);
    const parkedCaravans = caravans.filter((caravan) => caravan.parked);
    const problems = status?.analysis?.fleet?.problems ?? [];
    const activeMembers = new Set(caravans.flatMap((caravan) => caravan.members));
    const saloon = prValues(status).filter((pr) => openPr(pr) && !activeMembers.has(pr.number));
    const updated = repo.refreshed_unix_ms ? new Date(repo.refreshed_unix_ms).toLocaleTimeString() : "Never";
    const mode = status?.rebase_on_join?.state ?? "unknown";
    const webhook = state?.webhook;
    const webhookText = webhook?.enabled
      ? `webhook ${webhook.sync_enabled ? "sync" : "refresh"} · ${webhook.accepted} accepted · ${webhook.deduplicated} deduped · ${webhook.rejected} rejected`
      : "webhook disabled";
    ui.overview.innerHTML = `
      <article class="overview-card primary">
        <div><p class="eyebrow">${repo.config_existed ? "Configured repository" : "Default policy"}</p><h2 id="repo-title">${escapeHtml(repoName(repo))}</h2></div>
        <p>Updated ${escapeHtml(updated)} · physical chains ${escapeHtml(mode)} · ${escapeHtml(webhookText)}</p>
      </article>
      <article class="overview-card"><span class="metric-label">Caravans</span><strong class="metric">${activeCaravans.length}</strong><p>${parkedCaravans.length} parked · ${activeMembers.size} PRs</p></article>
      <article class="overview-card"><span class="metric-label">Saloon</span><strong class="metric">${saloon.length}</strong><p>not yet joined</p></article>
      <article class="overview-card"><span class="metric-label">Attention</span><strong class="metric">${problems.length + (repo.error ? 1 : 0)}</strong><p>${state.read_only ? "read only" : "mutable"}</p></article>`;
  }

  function actionButton(label, action, input, tone = "", mutates = true, auditRequired = false) {
    const actionBusy = selected()?.actions?.some((job) => ["queued", "running"].includes(job.state)) ?? false;
    const disabled = actionBusy || (state?.read_only && mutates);
    const title = actionBusy ? "Another repository action is in progress" : disabled ? "Disabled by --read-only" : `Run typed ${action.replaceAll("_", " ")} action`;
    return `<button type="button" class="mini-action ${tone}" data-web-action="${escapeHtml(action)}" data-web-input="${escapeHtml(JSON.stringify(input))}" data-mutates="${mutates}" ${auditRequired ? 'data-audit-required="true"' : ""} title="${escapeHtml(title)}" ${disabled ? "disabled" : ""}>${escapeHtml(label)}</button>`;
  }

  function renderPrCard(pr, index, status, pause) {
    if (!pr) return `<article class="pr-card"><p>Provider snapshot unavailable</p></article>`;
    const [ciText, ciTone] = checkTone(pr.checks);
    const auto = pr.auto_merge?.enabled ? badge("Auto merge", "info") : badge("Auto merge off");
    const forcePresent = hasLabel(pr, "caravan-force");
    const force = forcePresent ? badge("Force intent", "bad") : "";
    const failures = checkRows(pr.checks);
    const forceProblem = (status?.analysis?.fleet?.problems ?? []).some((problem) => (problem.prs ?? []).includes(pr.number));
    const forceEligible = index === 0 && openPr(pr) && !pause && !forceProblem && selected()?.effective_config?.force_merge === true && ciText !== "CI green";
    const forceControl = index === 0 && forcePresent
      ? actionButton("Unforce", "force_revoke", { pr: pr.number }, "", true, true)
      : forceEligible
        ? actionButton("Force", "force_arm", { pr: pr.number }, "danger", true, true)
        : "";
    return `<article class="pr-card ${index === 0 ? "head" : ""}">
      <div class="pr-kicker">${prAnchor(pr, `PR #${pr.number}`, "pr-number")}${index === 0 ? badge("Head", "info") : badge(`Position ${index + 1}`)}</div>
      <h3 class="pr-title">${prAnchor(pr, pr.title || `Pull request #${pr.number}`, "pr-title-link")}</h3>
      <div class="branch-line"><span>head</span><code title="${escapeHtml(pr.head?.name)}">${escapeHtml(pr.head?.name)}@${shortOid(pr.head?.oid)}</code></div>
      <div class="branch-line"><span>base</span><code title="${escapeHtml(pr.base?.name)}">${escapeHtml(pr.base?.name)}@${shortOid(pr.base?.oid)}</code></div>
      <div class="badges">${badge(ciText, ciTone)}${auto}${force}</div>
      ${failures ? `<details class="check-details"><summary>Why CI is blocked</summary>${failures}</details>` : ""}
      <div class="card-actions">
        ${actionButton("Check", "check", { pr: pr.number }, "", false)}
        ${forceControl}
        ${index > 0 ? actionButton("Split", "split", { pr: pr.number }) : ""}
        ${actionButton("Evict", "evict", { pr: pr.number, reason: "Cara web operator eviction" }, "danger")}
      </div>
    </article>`;
  }

  function pauseFor(status, caravan) {
    // Stale and provider-retired holds are historical diagnostics only; they
    // never present as an active hold or imply auto-merge repair.
    return (status?.pauses ?? []).find((pause) => pause?.record?.caravan_head === caravan.id && !["stale", "retired"].includes(normalized(pause.state)));
  }

  function renderCaravans(repo) {
    const status = repo.status;
    const caravans = status?.analysis?.fleet?.caravans ?? [];
    if (!caravans.length) {
      ui.caravans.innerHTML = empty("No active caravans");
      return;
    }
    ui.caravans.innerHTML = caravans.map((caravan) => {
      const members = caravan.members.map((number) => exactPr(status, number));
      const head = members[0];
      const pause = pauseFor(status, caravan);
      const holdAction = pause
        ? actionButton("Resume", "resume", { head_pr: caravan.id, actor: "cara-web" }, "primary")
        : actionButton("Pause", "pause", { head_pr: caravan.id, actor: "cara-web", reason: "Operator pause from Cara web", expires_unix_secs: null, external_reference: null });
      return `<article class="caravan">
        <header class="caravan-header">
          <div class="caravan-title"><h3>Caravan #${caravan.id}</h3><span>${members.length} ${members.length === 1 ? "member" : "members"}</span></div>
          <div class="caravan-tools">
            <div class="badges">${caravan.parked ? badge("Parked red", "bad") : pause ? badge("Paused", "warn") : head?.auto_merge?.enabled ? badge("Head armed", "good") : badge("Head not armed", "warn")}</div>
            <div class="inline-actions">${actionButton("Plan", "plan_sync", { all: true, rerun_failed: false }, "", false)}${actionButton("Sync", "sync", { all: true, rerun_failed: false }, "primary")}${holdAction}</div>
          </div>
        </header>
        <div class="trail">${members.map((pr, index) => renderPrCard(pr, index, status, pause)).join("")}</div>
      </article>`;
    }).join("");
  }

  function admissionFact(status, kind, pr) {
    return (status?.admission?.[kind] ?? []).find((item) => item.pr === pr.number) ?? null;
  }

  function compatibilityFact(repo, pr) {
    return (repo?.candidate_compatibility ?? []).find((item) => item.pr === pr.number) ?? null;
  }

  function targetLabel(target) {
    return target.kind === "default_branch" ? "main" : target.tail_pr ? `PR #${target.tail_pr}` : target.target?.name || "tail";
  }

  function targetSets(repo, pr) {
    const projection = compatibilityFact(repo, pr);
    const targets = projection?.targets ?? [];
    return {
      projection,
      ready: targets.filter((target) => normalized(target.outcome) === "clean"),
      conflicting: targets.filter((target) => normalized(target.outcome) === "conflict"),
      unknown: targets.filter((target) => !target.outcome),
    };
  }

  function compatibilityBadges(repo, pr) {
    const { projection, ready, conflicting, unknown } = targetSets(repo, pr);
    if (!projection) return badge("Compatibility unknown", "warn");
    const rows = [];
    if (ready.length) rows.push(badge(`Ready (${ready.map(targetLabel).join(", ")})`, "good"));
    if (conflicting.length) rows.push(badge(`Conflicting (${conflicting.map(targetLabel).join(", ")})`, "bad"));
    if (unknown.length || !projection.complete || projection.targets_truncated) rows.push(badge("Checking/unknown targets", "warn"));
    return rows.join("");
  }

  function compatibilityRows(repo, pr) {
    const { projection } = targetSets(repo, pr);
    if (!projection) return "";
    return `<details class="check-details"><summary>Exact target compatibility</summary><div class="check-list">${projection.targets.map((target) => {
      const outcome = normalized(target.outcome);
      const tone = outcome === "clean" ? "good" : outcome === "conflict" ? "bad" : "warn";
      const paths = target.conflicting_paths?.length ? ` · ${target.conflicting_paths.join(", ")}` : "";
      const error = target.error ? ` · ${target.error.code}: ${target.error.message}` : "";
      return `<div class="check-row"><span>${escapeHtml(targetLabel(target))}<small>${escapeHtml(paths + error)}</small></span>${badge(target.outcome || "unknown", tone)}</div>`;
    }).join("")}</div><p class="reason">generation ${escapeHtml(projection.generation_fingerprint)}${projection.targets_truncated ? ` · ${projection.targets_truncated} targets omitted` : ""}</p></details>`;
  }

  function reasonForPr(status, pr) {
    if (pr.draft) return "Draft pull request";
    if (hasLabel(pr, "caravan-evicted")) return "Explicitly evicted; renew or rejoin after fresh validation";
    const rejection = admissionFact(status, "rejected", pr);
    if (rejection) return rejection.reason;
    const skipped = admissionFact(status, "skipped", pr);
    if (skipped) return skipped.reason;
    const candidate = admissionFact(status, "candidates", pr);
    if (candidate) return candidate.reason;
    if (!hasLabel(pr, "caravan")) return "Not enrolled; exact candidate and compatibility preflight required";
    return "Open but outside a valid active caravan; inspect topology problems";
  }

  function saloonClassification(repo, status, pr) {
    if (admissionFact(status, "candidates", pr) && !pr.draft) {
      const { projection, ready, conflicting, unknown } = targetSets(repo, pr);
      if (ready.length) return "ready";
      if (projection?.complete && conflicting.length && !unknown.length) return "conflicting";
      return "other";
    }
    if (hasLabel(pr, "caravan-evicted") || hasLabel(pr, "caravan-join-skipped") || admissionFact(status, "skipped", pr)) return "bounty";
    const checkStates = currentChecks(pr.checks).map((check) => normalized(check.state));
    if (pr.draft || admissionFact(status, "rejected", pr) || checkStates.some((value) => value !== "success")) return "saddling";
    return "other";
  }

  function saloonGroupKey(repositoryId, name) {
    return `caravan.saloon.${repositoryId}.${name}`;
  }

  function saloonGroupOpen(repositoryId, name) {
    const retained = window.localStorage.getItem(saloonGroupKey(repositoryId, name));
    return retained === null
      ? name === "ready" || name === "conflicting" || name === "saddling"
      : retained === "open";
  }

  function saloonCard(repo, status, pr, group) {
    const { ready } = targetSets(repo, pr);
    const admissions = group === "ready" ? ready.map((target) => target.kind === "caravan_tail"
      ? actionButton(`Join #${target.tail_pr}`, "join", { pr: pr.number, tail_pr: target.tail_pr, create_pr: false, reason: "Caravan dashboard exact compatible admission", priority_label: null }, "primary")
      : actionButton("New caravan", "new", { pr: pr.number, create_pr: false, reason: "Caravan dashboard exact compatible admission", priority_label: null }, "primary")).join("") : "";
    const preflightTarget = ready.find((target) => target.tail_pr)?.tail_pr ?? (compatibilityFact(repo, pr)?.targets ?? []).find((target) => target.tail_pr)?.tail_pr;
    const groupTone = group === "ready" ? "good" : group === "conflicting" || group === "bounty" ? "bad" : "warn";
    const priorities = repo.effective_config?.agent_priority_labels ?? [];
    const selectedPriorities = priorities.filter((label) => hasLabel(pr, label));
    const unknownPriorities = labelNames(pr).filter((label) => label.startsWith("caravan-priority:") && !priorities.includes(label));
    const priorityEligible = openPr(pr) && !pr.draft && !pr.cross_repository && !hasLabel(pr, "caravan") && !hasLabel(pr, "caravan-evicted") && selectedPriorities.length <= 1 && !unknownPriorities.length;
    const priorityControls = priorityEligible ? priorities.map((label) => hasLabel(pr, label) ? "" : actionButton(label.replace("caravan-priority:", "Priority "), "priority_set", { pr: pr.number, label }, "", true, true)).join("") + (selectedPriorities.length ? actionButton("Priority FIFO", "priority_clear", { pr: pr.number }, "", true, true) : "") : "";
    const priorityBadge = selectedPriorities.length ? badge(selectedPriorities[0].replace("caravan-priority:", "Priority "), "info") : priorityEligible ? badge("Priority FIFO") : "";
    return `<article class="queue-card saloon-card">
      <div class="pr-kicker">${prAnchor(pr, `PR #${pr.number}`, "pr-number")}${pr.draft ? badge("Draft") : badge(SALOON_META[group][0], groupTone)}</div>
      <h3>${prAnchor(pr, pr.title || `Pull request #${pr.number}`, "pr-title-link")}</h3>
      <p><span class="mono">${escapeHtml(pr.head?.name)}@${shortOid(pr.head?.oid)}</span> → <span class="mono">${escapeHtml(pr.base?.name)}</span></p>
      <div class="badges">${compatibilityBadges(repo, pr)}${priorityBadge}</div>
      <p class="reason">${escapeHtml(reasonForPr(status, pr))}</p>
      ${compatibilityRows(repo, pr)}
      ${checkRows(pr.checks)}
      <div class="card-actions">${actionButton("Preflight", "check", { pr: pr.number, ...(preflightTarget ? { tail_pr: preflightTarget } : {}) }, "", false)}${admissions}${priorityControls}</div>
    </article>`;
  }

  function renderSaloon(repo) {
    const status = repo.status;
    const active = new Set((status?.analysis?.fleet?.caravans ?? []).flatMap((item) => item.members));
    const saloon = prValues(status).filter((pr) => openPr(pr) && !active.has(pr.number));
    ui.saloonCount.textContent = `${saloon.length} ${saloon.length === 1 ? "PR" : "PRs"}`;
    if (!saloon.length) {
      ui.saloon.innerHTML = empty("The Saloon is empty");
      return;
    }
    const candidateOrder = new Map((status?.admission?.candidates ?? []).map((item, index) => [item.pr, index]));
    saloon.sort((left, right) => (candidateOrder.get(left.number) ?? Number.MAX_SAFE_INTEGER) - (candidateOrder.get(right.number) ?? Number.MAX_SAFE_INTEGER) || left.number - right.number);
    const groups = Object.fromEntries(SALOON_ORDER.map((name) => [name, []]));
    saloon.forEach((pr) => groups[saloonClassification(repo, status, pr)].push(pr));
    ui.saloon.innerHTML = SALOON_ORDER.map((name) => {
      const [title, description] = SALOON_META[name];
      const rows = groups[name];
      const open = saloonGroupOpen(repo.id, name);
      return `<details class="saloon-group" data-saloon-group="${name}" ${open ? "open" : ""}>
        <summary><span><strong>${title}</strong><small>${description}</small></span>${badge(rows.length, rows.length ? (name === "ready" ? "good" : name === "conflicting" || name === "bounty" ? "bad" : "warn") : "")}</summary>
        <div class="saloon-cards">${rows.length ? rows.map((pr) => saloonCard(repo, status, pr, name)).join("") : empty(`No PRs in ${title}`)}</div>
      </details>`;
    }).join("");
  }

  function renderDecisions(repo) {
    const status = repo.status;
    const items = [];
    if (repo.error) items.push({ title: repo.error.code, message: repo.error.message, details: repo.error.details, tone: "bad" });
    (status?.analysis?.fleet?.problems ?? []).forEach((problem) => items.push({
      title: String(problem.kind || "problem").replaceAll("_", " "),
      message: `${problem.message}${problem.prs?.length ? ` · PRs ${problem.prs.map((item) => `#${item}`).join(", ")}` : ""}`,
      details: problem,
      tone: problem.kind === "unknown" ? "warn" : "bad",
    }));
    ui.attentionCount.textContent = String(items.length);
    if (!items.length) {
      ui.decisions.innerHTML = empty("No unresolved decisions");
      return;
    }
    ui.decisions.innerHTML = items.map((item) => `<article class="decision-card">
      <div class="pr-kicker">${badge(item.tone === "bad" ? "Decision" : "Review", item.tone)}</div>
      <h3>${escapeHtml(item.title)}</h3><p>${escapeHtml(item.message)}</p>
      ${item.details ? `<details><summary>Exact details</summary><pre>${escapeHtml(JSON.stringify(item.details, null, 2))}</pre></details>` : ""}
    </article>`).join("");
  }

  function diagnosticRows(result) {
    const observations = result?.ci ?? [];
    return observations.flatMap((observation) => (observation.failure_diagnostics ?? []).map((failure) => `
      <article class="evidence-card bad">
        <div class="evidence-title"><strong>PR #${observation.pr}</strong>${badge(failure.classification || "failure", "bad")}</div>
        <p>${escapeHtml((failure.reasons ?? []).join(" · ") || failure.action || "CI failure")}</p>
        <details><summary>Jobs, steps &amp; generation</summary><pre>${escapeHtml(JSON.stringify(failure, null, 2))}</pre></details>
      </article>`));
  }

  function renderEvidence(repo) {
    const failures = prValues(repo.status).flatMap((pr) => {
      const rows = checkRows(pr.checks);
      return rows ? [`<article class="evidence-card"><div class="evidence-title">${prAnchor(pr, `PR #${pr.number}`, "pr-number")} ${badge(checkTone(pr.checks)[0], checkTone(pr.checks)[1])}</div>${rows}</article>`] : [];
    });
    const action = repo.last_action;
    const result = action?.result;
    const actionDiagnostics = diagnosticRows(result);
    const events = result?.events ?? [];
    const deliveries = result?.hook_deliveries ?? [];
    const hooks = repo.effective_config?.hooks ?? {};
    const jobs = [...(repo.actions ?? [])].reverse();
    const journal = repo.journal?.records ?? [];
    const sections = [];
    if (jobs.length) sections.push(`<section class="inspector-section"><h3>Action progress</h3>${jobs.map((job) => `<article class="evidence-card ${job.state === "failed" ? "bad" : ""}"><div class="evidence-title"><strong>${escapeHtml(job.action)}</strong>${badge(job.state, job.state === "succeeded" ? "good" : job.state === "failed" ? "bad" : "warn")}</div><p>${escapeHtml(job.phase)} · ${new Date(job.updated_unix_ms).toLocaleTimeString()}</p>${job.checkpoint ? `<details open><summary>Durable checkpoint</summary><pre>${escapeHtml(JSON.stringify(job.checkpoint, null, 2))}</pre></details>` : ""}${job.error ? `<p>${escapeHtml(job.error.code)}: ${escapeHtml(job.error.message)}</p>` : ""}</article>`).join("")}</section>`);
    if (action) sections.push(`<section class="inspector-section"><h3>Last action · ${escapeHtml(action.action)}</h3>
      <p>${new Date(action.completed_unix_ms).toLocaleString()} · ${action.ok ? badge("Completed", "good") : badge("Failed", "bad")}</p>
      ${action.error ? `<article class="evidence-card bad"><strong>${escapeHtml(action.error.code)}</strong><p>${escapeHtml(action.error.message)}</p><details><summary>Structured continuation</summary><pre>${escapeHtml(JSON.stringify(action.error.details ?? {}, null, 2))}</pre></details></article>` : ""}
      ${result?.mutated === false && result?.actions ? `<article class="evidence-card plan-summary"><div class="evidence-title"><strong>No-write sync plan</strong>${badge(result.plan_hash || "exact", "info")}</div><p>${result.actions.length} ordered actions · ${result.decisions?.length ?? 0} decisions · provider writes ${result.provider_writes}</p></article>${result.actions.map((item) => `<article class="evidence-card"><div class="evidence-title"><strong>${item.order}. ${escapeHtml(item.kind)}</strong>${badge(item.state, item.state === "would_mutate" ? "warn" : item.state === "would_stop" ? "bad" : "info")}</div><p>${escapeHtml(item.reason)}</p>${item.pr ? `<p>PR #${item.pr}${item.caravan_id ? ` · caravan #${item.caravan_id}` : ""}</p>` : ""}<details><summary>Exact precondition &amp; target</summary><pre>${escapeHtml(JSON.stringify({expected: item.expected, target: item.target, phase: item.phase}, null, 2))}</pre></details></article>`).join("")}${(result.decisions ?? []).map((item) => `<article class="evidence-card bad"><strong>${escapeHtml(item.code)}</strong><p>${escapeHtml(item.reason)}</p><p>Next: ${escapeHtml(item.next)}</p></article>`).join("")}` : ""}
      ${actionDiagnostics.join("")}
      ${result?.scheduler_status ? `<article class="evidence-card"><strong>Scheduler</strong><p>${escapeHtml(result.scheduler_status.disposition)} · ${escapeHtml(result.scheduler_status.reason)}</p></article>` : ""}
    </section>`);
    sections.push(`<section class="inspector-section"><h3>Current CI</h3>${failures.length ? failures.join("") : empty("No failing or pending checks")}</section>`);
    sections.push(`<section class="inspector-section"><h3>Events &amp; hooks</h3>
      ${events.length ? events.map((event) => `<article class="evidence-card"><div class="evidence-title"><strong>${escapeHtml(event.kind)}</strong><code>${escapeHtml(event.event_id)}</code></div><p>${escapeHtml(event.reason || `PRs ${(event.prs ?? []).join(", ")}`)}</p></article>`).join("") : ""}
      ${deliveries.length ? deliveries.map((delivery) => `<article class="evidence-card"><div class="evidence-title"><strong>${escapeHtml(delivery.kind)}</strong>${badge(delivery.state, normalized(delivery.state) === "succeeded" ? "good" : "bad")}</div><p>${delivery.blocking ? "Blocking" : "Best effort"} · exit ${escapeHtml(delivery.exit_code ?? "none")} · stdout ${delivery.stdout_bytes} B · stderr ${delivery.stderr_bytes} B</p></article>`).join("") : ""}
      ${journal.length ? journal.slice().reverse().map((record) => { const event = record.event; const delivery = record.delivery; return `<article class="evidence-card"><div class="evidence-title"><strong>${escapeHtml(event?.kind || record.kind || "journal")}</strong>${delivery ? badge(delivery.state, normalized(delivery.state) === "succeeded" ? "good" : "bad") : badge("event", "info")}</div><p>${escapeHtml(event?.reason || (event?.prs?.length ? `PRs ${event.prs.join(", ")}` : record.timestamp || "durable Cara journal"))}</p><details><summary>Journal receipt</summary><pre>${escapeHtml(JSON.stringify(record, null, 2))}</pre></details></article>`; }).join("") : ""}
      ${!events.length && !deliveries.length && !journal.length ? empty("No durable Cara journal records yet") : ""}
      <details class="hook-config"><summary>${Object.keys(hooks).length} configured hook policies</summary><pre>${escapeHtml(JSON.stringify(hooks, null, 2))}</pre></details>
    </section>`);
    ui.inspectorContent.innerHTML = sections.join("");
  }

  function renderConfig(repo) {
    ui.inspectorContent.innerHTML = `<section class="inspector-section config-section">
      <div class="config-meta"><span>${repo.config_existed ? "Repository config" : "Effective defaults"}</span><code>${escapeHtml(repo.config_path)}</code></div>
      <pre class="config-view">${escapeHtml(JSON.stringify(repo.effective_config ?? {}, null, 2))}</pre>
    </section>`;
  }

  function openInspector(mode) {
    const repo = selected();
    if (!repo) return;
    inspectorMode = mode;
    ui.inspector.hidden = false;
    ui.inspectorKicker.textContent = repoName(repo);
    ui.inspectorTitle.textContent = mode === "config" ? "Effective configuration" : "Operational evidence";
    if (mode === "config") renderConfig(repo); else renderEvidence(repo);
  }

  function renderConcatControl(repo, actionBusy) {
    const caravans = (repo.status?.analysis?.fleet?.caravans ?? []).filter((caravan) => !caravan.parked);
    ui.concatControl.hidden = caravans.length < 2;
    if (caravans.length < 2) {
      concatPlans.delete(repo.id);
      return;
    }
    const sourceValue = ui.concatSource.value;
    const targetValue = ui.concatTarget.value;
    ui.concatSource.innerHTML = caravans.map((caravan) => `<option value="${caravan.id}">Caravan #${caravan.id} · ${caravan.members.length} member(s)</option>`).join("");
    ui.concatTarget.innerHTML = caravans.map((caravan) => {
      const tail = caravan.members.at(-1);
      return `<option value="${tail}">Caravan #${caravan.id} tail #${tail}</option>`;
    }).join("");
    if ([...ui.concatSource.options].some((option) => option.value === sourceValue)) ui.concatSource.value = sourceValue;
    if ([...ui.concatTarget.options].some((option) => option.value === targetValue)) ui.concatTarget.value = targetValue;
    const reviewed = concatPlans.get(repo.id);
    const currentPlan = reviewed && reviewed.refreshSequence === repo.refresh_sequence ? reviewed.plan : null;
    if (!currentPlan && reviewed) concatPlans.delete(repo.id);
    ui.concatPlanHash.textContent = currentPlan ? `Reviewed ${currentPlan.plan_hash}` : "No reviewed plan";
    ui.planConcat.disabled = actionBusy;
    ui.executeConcat.disabled = !currentPlan || actionBusy || state.read_only || state.hosted;
  }

  function render() {
    if (!state?.repositories?.length) return;
    if (!selectedRepo || !state.repositories.some((repo) => repo.id === selectedRepo)) selectedRepo = state.repositories[0].id;
    const repo = selected();
    const hasCaravans = (repo.status?.analysis?.fleet?.caravans ?? []).length > 0;
    ui.dashboard.classList.toggle("no-caravans", !hasCaravans);
    ui.dashboard.hidden = false;
    ui.plan.hidden = false;
    ui.sync.hidden = false;
    const actionBusy = repo.actions?.some((job) => ["queued", "running"].includes(job.state)) ?? false;
    ui.plan.disabled = actionBusy;
    ui.sync.disabled = actionBusy || state.read_only;
    ui.evidence.hidden = false;
    ui.config.hidden = false;
    renderTabs();
    renderOverview(repo);
    renderCaravans(repo);
    renderConcatControl(repo, actionBusy);
    renderSaloon(repo);
    renderDecisions(repo);
    applySidebarState();
    if (inspectorMode) openInspector(inspectorMode);
  }

  function toast(message) {
    const node = document.createElement("div");
    node.className = "toast";
    node.textContent = message;
    ui.toasts.append(node);
    setTimeout(() => node.remove(), 4500);
  }

  async function fetchState({ quiet = false } = {}) {
    if (requestInFlight) return;
    requestInFlight = true;
    setBusy(true);
    try {
      const response = await fetch("/api/v1/state", { headers: { Accept: "application/json" }, cache: "no-store" });
      if (!response.ok) throw new Error(`state request failed (${response.status})`);
      state = await response.json();
      ui.connection.textContent = "Live";
      ui.connection.className = "connection online";
      render();
    } catch (error) {
      ui.connection.textContent = "Disconnected";
      ui.connection.className = "connection offline";
      if (!quiet) toast(error.message);
    } finally {
      requestInFlight = false;
      setBusy(false);
    }
  }

  async function performAction(action, input, button) {
    const repo = selected();
    if (!repo || (state.read_only && button?.dataset.mutates !== "false")) return;
    const destructive = ["concat", "evict", "split", "pause", "repair_abort", "force_arm", "force_revoke", "priority_set", "priority_clear"].includes(action);
    if (destructive && !window.confirm(`Run ${action.replaceAll("_", " ")} against ${repoName(repo)} using the current exact snapshot?`)) return;
    if (button) button.disabled = true;
    setBusy(true);
    try {
      const response = await fetch(`/api/v1/repos/${encodeURIComponent(repo.id)}/action`, {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Cara-CSRF": state.csrf_token, Accept: "application/json" },
        body: JSON.stringify({ expected_refresh_sequence: repo.refresh_sequence, action, input }),
      });
      const payload = await response.json();
      if (!payload.ok) throw new Error(payload.error?.message || `${action} was not accepted`);
      repo.actions = [...(repo.actions ?? []), payload.job];
      render();
      openInspector("evidence");
      toast(`${action.replaceAll("_", " ")} started`);
      await pollAction(repo.id, payload.action_id);
    } catch (error) {
      toast(error.message);
      await fetchState({ quiet: true });
      openInspector("evidence");
    } finally {
      if (button) button.disabled = state.read_only && button.dataset.mutates !== "false";
      setBusy(false);
    }
  }

  async function pollAction(repositoryId, actionId) {
    for (let attempt = 0; attempt < 2400; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 750));
      await fetchState({ quiet: true });
      const repo = state?.repositories?.find((item) => item.id === repositoryId);
      const job = repo?.actions?.find((item) => item.id === actionId);
      if (!job) continue;
      openInspector("evidence");
      if (["succeeded", "failed"].includes(job.state)) {
        if (job.state === "succeeded" && job.action === "plan_concat" && repo.last_action?.result?.plan_hash) {
          concatPlans.set(repo.id, { plan: repo.last_action.result, refreshSequence: repo.refresh_sequence });
          render();
          openInspector("evidence");
        }
        if (job.state === "succeeded" && job.action === "concat") concatPlans.delete(repo.id);
        toast(job.state === "succeeded" ? `${job.action.replaceAll("_", " ")} completed` : job.error?.message || `${job.action} failed`);
        return;
      }
    }
    toast("Action is still running; progress remains available in Evidence");
  }

  async function refreshAll() {
    if (!state) return fetchState();
    setBusy(true);
    try {
      await Promise.all(state.repositories.map((repo) => fetch(`/api/v1/repos/${encodeURIComponent(repo.id)}/refresh`, {
        method: "POST",
        headers: { "X-Cara-CSRF": state.csrf_token, Accept: "application/json" },
      })));
      await fetchState();
      toast("Snapshots refreshed");
    } catch (error) {
      toast(error.message);
    } finally {
      setBusy(false);
    }
  }

  ui.refresh.addEventListener("click", refreshAll);
  function concatIntent() {
    return {
      source_head_pr: Number(ui.concatSource.value),
      target_tail_pr: Number(ui.concatTarget.value),
      actor: ui.concatActor.value,
      reason: ui.concatReason.value,
    };
  }

  function invalidateConcatPlan() {
    if (selectedRepo) concatPlans.delete(selectedRepo);
    render();
  }

  ui.plan.addEventListener("click", () => performAction("plan_sync", { all: true, rerun_failed: false }, ui.plan));
  ui.sync.addEventListener("click", () => performAction("sync", { all: true, rerun_failed: false }, ui.sync));
  ui.planConcat.addEventListener("click", () => performAction("plan_concat", concatIntent(), ui.planConcat));
  ui.executeConcat.addEventListener("click", () => {
    const reviewed = concatPlans.get(selectedRepo);
    if (!reviewed) return;
    performAction("concat", { ...concatIntent(), expected_plan_hash: reviewed.plan.plan_hash }, ui.executeConcat);
  });
  [ui.concatSource, ui.concatTarget, ui.concatActor, ui.concatReason].forEach((control) => control.addEventListener("change", invalidateConcatPlan));
  ui.evidence.addEventListener("click", () => openInspector("evidence"));
  ui.config.addEventListener("click", () => openInspector("config"));
  ui.closeInspector.addEventListener("click", () => { inspectorMode = null; ui.inspector.hidden = true; });
  ui.toggleRepositories.addEventListener("click", () => toggleSidebar("repositories"));
  ui.toggleAttention.addEventListener("click", () => toggleSidebar("attention"));
  ui.workspace.addEventListener("click", (event) => {
    const button = event.target.closest("[data-close-sidebar]");
    if (button) toggleSidebar(button.dataset.closeSidebar, false);
  });
  ui.tabs.addEventListener("click", (event) => {
    const button = event.target.closest("[data-repo]");
    if (!button) return;
    selectedRepo = button.dataset.repo;
    render();
  });
  ui.saloon.addEventListener("toggle", (event) => {
    const group = event.target.closest?.("[data-saloon-group]");
    if (!group || !selectedRepo) return;
    window.localStorage.setItem(
      saloonGroupKey(selectedRepo, group.dataset.saloonGroup),
      group.open ? "open" : "closed",
    );
  }, true);
  ui.dashboard.addEventListener("click", (event) => {
    const button = event.target.closest("[data-web-action]");
    if (!button) return;
    let input = {};
    try { input = JSON.parse(button.dataset.webInput || "{}"); } catch { toast("Invalid embedded action payload"); return; }
    if (button.dataset.auditRequired === "true") {
      const defaultActor = window.localStorage.getItem("caravan.audit.actor") || "cara-web";
      const actor = window.prompt("Audited actor", defaultActor);
      if (actor === null) return;
      const reason = window.prompt(`Reason for ${button.dataset.webAction.replaceAll("_", " ")}`, "");
      if (reason === null) return;
      if (!actor.trim() || !reason.trim()) { toast("Actor and reason are required"); return; }
      window.localStorage.setItem("caravan.audit.actor", actor.trim());
      input = { ...input, actor: actor.trim(), reason: reason.trim() };
    }
    performAction(button.dataset.webAction, input, button);
  });
  applySidebarState();
  fetchState();
  setInterval(() => fetchState({ quiet: true }), 5000);
})();
