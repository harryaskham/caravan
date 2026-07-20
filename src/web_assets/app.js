(() => {
  "use strict";

  const ui = {
    tabs: document.querySelector("#repo-tabs"),
    dashboard: document.querySelector("#dashboard"),
    overview: document.querySelector("#repo-overview"),
    caravans: document.querySelector("#caravans"),
    waiting: document.querySelector("#waiting-prs"),
    decisions: document.querySelector("#decisions"),
    connection: document.querySelector("#connection"),
    activity: document.querySelector("#activity"),
    refresh: document.querySelector("#refresh-all"),
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

  function empty(message) {
    const node = ui.empty.content.firstElementChild.cloneNode(true);
    node.querySelector("p").textContent = message;
    return node.outerHTML;
  }

  function badge(text, tone = "") {
    return `<span class="badge ${tone}">${escapeHtml(text)}</span>`;
  }

  function setBusy(busy) {
    ui.activity.hidden = !busy;
    ui.refresh.disabled = busy;
    ui.connection.setAttribute("aria-busy", String(busy));
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

  function checkTone(checks) {
    if (!checks?.length) return ["No checks", "warn"];
    const states = checks.map((check) => normalized(check.state));
    if (states.some((item) => ["failure", "error", "cancelled", "timed_out", "unknown"].includes(item))) return ["CI blocked", "bad"];
    if (states.some((item) => ["queued", "pending", "in_progress", "expected"].includes(item))) return ["CI running", "warn"];
    return ["CI green", "good"];
  }

  function checkRows(checks, showPassing = false) {
    const rows = (checks ?? []).filter((check) => showPassing || normalized(check.state) !== "success");
    if (!rows.length) return "";
    return `<div class="check-list">${rows.map((check) => {
      const stateName = normalized(check.state);
      const tone = stateName === "success" ? "good" : ["queued", "pending", "in_progress", "expected"].includes(stateName) ? "warn" : "bad";
      const name = check.details_url
        ? `<a href="${escapeHtml(check.details_url)}" target="_blank" rel="noopener noreferrer">${escapeHtml(check.name)}</a>`
        : `<span>${escapeHtml(check.name)}</span>`;
      return `<div class="check-row">${name}${badge(check.provider_state || stateName, tone)}</div>`;
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
    const problems = status?.analysis?.fleet?.problems ?? [];
    const activeMembers = new Set(caravans.flatMap((caravan) => caravan.members));
    const waiting = prValues(status).filter((pr) => openPr(pr) && !activeMembers.has(pr.number));
    const updated = repo.refreshed_unix_ms ? new Date(repo.refreshed_unix_ms).toLocaleTimeString() : "Never";
    const mode = status?.rebase_on_join?.state ?? "unknown";
    ui.overview.innerHTML = `
      <article class="overview-card primary">
        <div><p class="eyebrow">${repo.config_existed ? "Configured repository" : "Default policy"}</p><h2 id="repo-title">${escapeHtml(repoName(repo))}</h2></div>
        <p>Updated ${escapeHtml(updated)} · physical chains ${escapeHtml(mode)}</p>
      </article>
      <article class="overview-card"><span class="metric-label">Caravans</span><strong class="metric">${caravans.length}</strong><p>${activeMembers.size} PRs</p></article>
      <article class="overview-card"><span class="metric-label">Waiting</span><strong class="metric">${waiting.length}</strong><p>not enrolled</p></article>
      <article class="overview-card"><span class="metric-label">Attention</span><strong class="metric">${problems.length + (repo.error ? 1 : 0)}</strong><p>${state.read_only ? "read only" : "mutable"}</p></article>`;
  }

  function actionButton(label, action, input, tone = "", mutates = true) {
    const disabled = state?.read_only && mutates;
    const title = disabled ? "Disabled by --read-only" : `Run typed ${action.replaceAll("_", " ")} action`;
    return `<button type="button" class="mini-action ${tone}" data-web-action="${escapeHtml(action)}" data-web-input="${escapeHtml(JSON.stringify(input))}" data-mutates="${mutates}" title="${escapeHtml(title)}" ${disabled ? "disabled" : ""}>${escapeHtml(label)}</button>`;
  }

  function renderPrCard(pr, index) {
    if (!pr) return `<article class="pr-card"><p>Provider snapshot unavailable</p></article>`;
    const [ciText, ciTone] = checkTone(pr.checks);
    const auto = pr.auto_merge?.enabled ? badge("Auto merge", "info") : badge("Auto merge off");
    const force = hasLabel(pr, "caravan-force") ? badge("Force intent", "bad") : "";
    const failures = checkRows(pr.checks);
    return `<article class="pr-card ${index === 0 ? "head" : ""}">
      <div class="pr-kicker">${prAnchor(pr, `PR #${pr.number}`, "pr-number")}${index === 0 ? badge("Head", "info") : badge(`Position ${index + 1}`)}</div>
      <h3 class="pr-title">${prAnchor(pr, pr.title || `Pull request #${pr.number}`, "pr-title-link")}</h3>
      <div class="branch-line"><span>head</span><code title="${escapeHtml(pr.head?.name)}">${escapeHtml(pr.head?.name)}@${shortOid(pr.head?.oid)}</code></div>
      <div class="branch-line"><span>base</span><code title="${escapeHtml(pr.base?.name)}">${escapeHtml(pr.base?.name)}@${shortOid(pr.base?.oid)}</code></div>
      <div class="badges">${badge(ciText, ciTone)}${auto}${force}</div>
      ${failures ? `<details class="check-details"><summary>Why CI is blocked</summary>${failures}</details>` : ""}
      <div class="card-actions">
        ${actionButton("Check", "check", { pr: pr.number }, "", false)}
        ${index > 0 ? actionButton("Split", "split", { pr: pr.number }) : ""}
        ${actionButton("Evict", "evict", { pr: pr.number, reason: "Cara web operator eviction" }, "danger")}
      </div>
    </article>`;
  }

  function pauseFor(status, caravan) {
    return (status?.pauses ?? []).find((pause) => pause?.record?.caravan_head === caravan.id && normalized(pause.state) !== "stale");
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
            <div class="badges">${pause ? badge("Paused", "warn") : head?.auto_merge?.enabled ? badge("Head armed", "good") : badge("Head not armed", "warn")}</div>
            <div class="inline-actions">${actionButton("Sync", "sync", { all: true, rerun_failed: false }, "primary")}${holdAction}</div>
          </div>
        </header>
        <div class="trail">${members.map(renderPrCard).join("")}</div>
      </article>`;
    }).join("");
  }

  function reasonForPr(status, pr) {
    if (pr.draft) return "Draft pull request";
    if (hasLabel(pr, "caravan-evicted")) return "Explicitly evicted; renew or rejoin after fresh validation";
    const rejection = status?.admission?.rejected?.find((item) => item.pr === pr.number);
    if (rejection) return rejection.reason;
    const skipped = status?.admission?.skipped?.find((item) => item.pr === pr.number);
    if (skipped) return skipped.reason;
    const candidate = status?.admission?.candidates?.find((item) => item.pr === pr.number);
    if (candidate) return candidate.reason;
    if (!hasLabel(pr, "caravan")) return "Not enrolled; exact candidate and compatibility preflight required";
    return "Open but outside a valid active caravan; inspect topology problems";
  }

  function renderWaiting(repo) {
    const status = repo.status;
    const active = new Set((status?.analysis?.fleet?.caravans ?? []).flatMap((item) => item.members));
    const waiting = prValues(status).filter((pr) => openPr(pr) && !active.has(pr.number));
    if (!waiting.length) {
      ui.waiting.innerHTML = empty("No waiting pull requests");
      return;
    }
    const firstCaravan = status?.analysis?.fleet?.caravans?.[0];
    const tail = firstCaravan?.members?.at(-1);
    ui.waiting.innerHTML = waiting.map((pr) => {
      const admission = hasLabel(pr, "caravan-evicted")
        ? tail ? actionButton("Rejoin", "rejoin", { pr: pr.number, tail_pr: tail, create_pr: false, reason: "Cara web operator rejoin", priority_label: null }, "primary") : actionButton("Renew", "renew", { pr: pr.number, create_pr: false, reason: "Cara web operator renew", priority_label: null }, "primary")
        : tail ? actionButton("Join tail", "join", { pr: pr.number, tail_pr: tail, create_pr: false, reason: "Cara web canonical admission", priority_label: null }, "primary") : actionButton("New caravan", "new", { pr: pr.number, create_pr: false, reason: "Cara web canonical admission", priority_label: null }, "primary");
      return `<article class="queue-card">
        <div class="pr-kicker">${prAnchor(pr, `PR #${pr.number}`, "pr-number")}${pr.draft ? badge("Draft") : badge("Open", "info")}</div>
        <h3>${prAnchor(pr, pr.title || `Pull request #${pr.number}`, "pr-title-link")}</h3>
        <p><span class="mono">${escapeHtml(pr.head?.name)}@${shortOid(pr.head?.oid)}</span> → <span class="mono">${escapeHtml(pr.base?.name)}</span></p>
        <p class="reason">${escapeHtml(reasonForPr(status, pr))}</p>
        ${checkRows(pr.checks)}
        <div class="card-actions">${actionButton("Preflight", "check", { pr: pr.number, ...(tail ? { tail_pr: tail } : {}) }, "", false)}${admission}</div>
      </article>`;
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
    const sections = [];
    if (action) sections.push(`<section class="inspector-section"><h3>Last action · ${escapeHtml(action.action)}</h3>
      <p>${new Date(action.completed_unix_ms).toLocaleString()} · ${action.ok ? badge("Completed", "good") : badge("Failed", "bad")}</p>
      ${action.error ? `<article class="evidence-card bad"><strong>${escapeHtml(action.error.code)}</strong><p>${escapeHtml(action.error.message)}</p><details><summary>Structured continuation</summary><pre>${escapeHtml(JSON.stringify(action.error.details ?? {}, null, 2))}</pre></details></article>` : ""}
      ${actionDiagnostics.join("")}
      ${result?.scheduler_status ? `<article class="evidence-card"><strong>Scheduler</strong><p>${escapeHtml(result.scheduler_status.disposition)} · ${escapeHtml(result.scheduler_status.reason)}</p></article>` : ""}
    </section>`);
    sections.push(`<section class="inspector-section"><h3>Current CI</h3>${failures.length ? failures.join("") : empty("No failing or pending checks")}</section>`);
    sections.push(`<section class="inspector-section"><h3>Events &amp; hooks</h3>
      ${events.length ? events.map((event) => `<article class="evidence-card"><div class="evidence-title"><strong>${escapeHtml(event.kind)}</strong><code>${escapeHtml(event.event_id)}</code></div><p>${escapeHtml(event.reason || `PRs ${(event.prs ?? []).join(", ")}`)}</p></article>`).join("") : ""}
      ${deliveries.length ? deliveries.map((delivery) => `<article class="evidence-card"><div class="evidence-title"><strong>${escapeHtml(delivery.kind)}</strong>${badge(delivery.state, normalized(delivery.state) === "succeeded" ? "good" : "bad")}</div><p>${delivery.blocking ? "Blocking" : "Best effort"} · exit ${escapeHtml(delivery.exit_code ?? "none")} · stdout ${delivery.stdout_bytes} B · stderr ${delivery.stderr_bytes} B</p></article>`).join("") : ""}
      ${!events.length && !deliveries.length ? empty("Run a typed action to retain its event and hook receipts") : ""}
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

  function render() {
    if (!state?.repositories?.length) return;
    if (!selectedRepo || !state.repositories.some((repo) => repo.id === selectedRepo)) selectedRepo = state.repositories[0].id;
    const repo = selected();
    const hasCaravans = (repo.status?.analysis?.fleet?.caravans ?? []).length > 0;
    ui.dashboard.classList.toggle("no-caravans", !hasCaravans);
    ui.dashboard.hidden = false;
    ui.sync.hidden = false;
    ui.sync.disabled = state.read_only;
    ui.evidence.hidden = false;
    ui.config.hidden = false;
    renderTabs();
    renderOverview(repo);
    renderCaravans(repo);
    renderWaiting(repo);
    renderDecisions(repo);
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
    const destructive = ["evict", "split", "pause", "repair_abort"].includes(action);
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
      if (payload.snapshot) Object.assign(repo, payload.snapshot);
      render();
      openInspector("evidence");
      if (!payload.ok) throw new Error(payload.error?.message || `${action} failed`);
      toast(`${action.replaceAll("_", " ")} completed`);
      await fetchState({ quiet: true });
    } catch (error) {
      toast(error.message);
      await fetchState({ quiet: true });
      openInspector("evidence");
    } finally {
      if (button) button.disabled = state.read_only && button.dataset.mutates !== "false";
      setBusy(false);
    }
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
  ui.sync.addEventListener("click", () => performAction("sync", { all: true, rerun_failed: false }, ui.sync));
  ui.evidence.addEventListener("click", () => openInspector("evidence"));
  ui.config.addEventListener("click", () => openInspector("config"));
  ui.closeInspector.addEventListener("click", () => { inspectorMode = null; ui.inspector.hidden = true; });
  ui.tabs.addEventListener("click", (event) => {
    const button = event.target.closest("[data-repo]");
    if (!button) return;
    selectedRepo = button.dataset.repo;
    render();
  });
  ui.dashboard.addEventListener("click", (event) => {
    const button = event.target.closest("[data-web-action]");
    if (!button) return;
    let input = {};
    try { input = JSON.parse(button.dataset.webInput || "{}"); } catch { toast("Invalid embedded action payload"); return; }
    performAction(button.dataset.webAction, input, button);
  });
  fetchState();
  setInterval(() => fetchState({ quiet: true }), 5000);
})();
