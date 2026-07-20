(() => {
  "use strict";

  const ui = {
    tabs: document.querySelector("#repo-tabs"),
    loading: document.querySelector("#loading"),
    dashboard: document.querySelector("#dashboard"),
    overview: document.querySelector("#repo-overview"),
    caravans: document.querySelector("#caravans"),
    waiting: document.querySelector("#waiting-prs"),
    decisions: document.querySelector("#decisions"),
    connection: document.querySelector("#connection"),
    refresh: document.querySelector("#refresh-all"),
    toasts: document.querySelector("#toast-region"),
    empty: document.querySelector("#empty-template"),
  };

  let state = null;
  let selectedRepo = null;
  let requestInFlight = false;

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
  const openPr = (pr) => pr?.state === "Open" || pr?.state === "open";

  function empty(message) {
    const node = ui.empty.content.firstElementChild.cloneNode(true);
    node.querySelector("p").textContent = message;
    return node.outerHTML;
  }

  function badge(text, tone = "") {
    return `<span class="badge ${tone}">${escapeHtml(text)}</span>`;
  }

  function checkTone(checks) {
    if (!checks?.length) return ["No checks", "warn"];
    const states = checks.map((check) => String(check.state ?? "unknown").toLowerCase());
    if (states.some((item) => ["failure", "error", "cancelled", "timed_out", "unknown"].includes(item))) return ["CI blocked", "bad"];
    if (states.some((item) => ["queued", "pending", "in_progress", "expected"].includes(item))) return ["CI running", "warn"];
    return ["CI green", "good"];
  }

  function repoName(repo) {
    const statusRepo = repo.status?.repository;
    if (statusRepo) return `${statusRepo.owner}/${statusRepo.name}`;
    return repo.path.split("/").filter(Boolean).at(-1) || repo.id;
  }

  function renderTabs() {
    ui.tabs.innerHTML = state.repositories.map((repo) => {
      const selected = repo.id === selectedRepo;
      const health = repo.error ? "bad" : repo.status?.healthy ? "good" : "";
      const detail = repo.error ? repo.error.code : repo.status?.healthy ? "Healthy" : "Needs attention";
      return `<button class="repo-tab" role="tab" aria-selected="${selected}" data-repo="${escapeHtml(repo.id)}">
        <strong>${escapeHtml(repoName(repo))}</strong>
        <small><span class="status-dot ${health}"></span>${escapeHtml(detail)}</small>
      </button>`;
    }).join("");
    ui.tabs.querySelectorAll("[data-repo]").forEach((button) => {
      button.addEventListener("click", () => {
        selectedRepo = button.dataset.repo;
        render();
      });
    });
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
    const rebase = status?.rebase_on_join?.state ?? "unknown";
    const scheduler = status?.scheduler_status?.disposition ?? (status?.healthy ? "healthy" : "attention");
    const updated = repo.refreshed_unix_ms ? new Date(repo.refreshed_unix_ms).toLocaleTimeString() : "Never";
    ui.overview.innerHTML = `
      <article class="overview-card primary">
        <p class="eyebrow">${escapeHtml(repo.path)}</p>
        <h2 id="repo-title">${escapeHtml(repoName(repo))}</h2>
        <p>Updated ${escapeHtml(updated)} · ${escapeHtml(scheduler)} · rebase_on_join=${escapeHtml(rebase)}</p>
      </article>
      <article class="overview-card"><span class="metric-label">Caravans</span><strong class="metric">${caravans.length}</strong><p>${activeMembers.size} enrolled pull requests</p></article>
      <article class="overview-card"><span class="metric-label">At the rail</span><strong class="metric">${waiting.length}</strong><p>Open pull requests not enrolled</p></article>
      <article class="overview-card"><span class="metric-label">Problems</span><strong class="metric">${problems.length}</strong><p>${state.read_only ? "Read-only dashboard" : "Actions require exact receipts"}</p></article>`;
  }

  function renderPrCard(pr, index) {
    if (!pr) return `<article class="pr-card"><p>Provider snapshot unavailable</p></article>`;
    const [ciText, ciTone] = checkTone(pr.checks);
    const auto = pr.auto_merge?.enabled ? badge("Auto merge", "info") : badge("Auto merge off");
    const force = hasLabel(pr, "caravan-force") ? badge("Force intent", "bad") : "";
    return `<article class="pr-card ${index === 0 ? "head" : ""}">
      <div class="pr-kicker"><span class="pr-number">PR #${pr.number}</span>${index === 0 ? badge("Head", "info") : badge(`Position ${index + 1}`)}</div>
      <h3 class="pr-title">${escapeHtml(pr.title || `Pull request #${pr.number}`)}</h3>
      <div class="branch-line"><span>head</span><code title="${escapeHtml(pr.head?.name)}">${escapeHtml(pr.head?.name)}@${shortOid(pr.head?.oid)}</code></div>
      <div class="branch-line"><span>base</span><code title="${escapeHtml(pr.base?.name)}">${escapeHtml(pr.base?.name)}@${shortOid(pr.base?.oid)}</code></div>
      <div class="badges">${badge(ciText, ciTone)}${auto}${force}</div>
    </article>`;
  }

  function renderCaravans(repo) {
    const status = repo.status;
    const caravans = status?.analysis?.fleet?.caravans ?? [];
    if (!caravans.length) {
      ui.caravans.innerHTML = empty("No active caravans. Canonical admission candidates remain visible below.");
      return;
    }
    ui.caravans.innerHTML = caravans.map((caravan) => {
      const members = caravan.members.map((number) => exactPr(status, number));
      const head = members[0];
      return `<article class="caravan">
        <header class="caravan-header">
          <div class="caravan-title"><h3>Caravan #${caravan.id}</h3><span>${members.length} ${members.length === 1 ? "member" : "members"}</span></div>
          <div class="badges">${head?.auto_merge?.enabled ? badge("Head armed", "good") : badge("Head not armed", "warn")}</div>
        </header>
        <div class="trail">${members.map(renderPrCard).join("")}</div>
      </article>`;
    }).join("");
  }

  function reasonForPr(status, pr) {
    if (pr.draft) return "Draft pull request; drafts are never automatically admitted.";
    if (hasLabel(pr, "caravan-evicted")) return "Explicitly evicted; use renew or rejoin after fresh validation.";
    const rejection = status?.admission?.rejected?.find((item) => item.pr === pr.number);
    if (rejection) return rejection.reason;
    const candidate = status?.admission?.candidates?.find((item) => item.pr === pr.number);
    if (candidate) return candidate.reason;
    if (!hasLabel(pr, "caravan")) return "Not enrolled. It must pass exact remote candidate and compatibility preflight.";
    return "Open but not part of a valid active caravan; inspect topology problems.";
  }

  function renderWaiting(repo) {
    const status = repo.status;
    const active = new Set((status?.analysis?.fleet?.caravans ?? []).flatMap((item) => item.members));
    const waiting = prValues(status).filter((pr) => openPr(pr) && !active.has(pr.number));
    if (!waiting.length) {
      ui.waiting.innerHTML = empty("Every visible open pull request is enrolled, or no candidates are currently open.");
      return;
    }
    ui.waiting.innerHTML = waiting.map((pr) => `
      <article class="queue-card">
        <div class="pr-kicker"><span class="pr-number">PR #${pr.number}</span>${pr.draft ? badge("Draft") : badge("Open", "info")}</div>
        <h3>${escapeHtml(pr.title || `Pull request #${pr.number}`)}</h3>
        <p><span class="mono">${escapeHtml(pr.head?.name)}@${shortOid(pr.head?.oid)}</span> → <span class="mono">${escapeHtml(pr.base?.name)}</span></p>
        <p class="reason">${escapeHtml(reasonForPr(status, pr))}</p>
      </article>`).join("");
  }

  function renderDecisions(repo) {
    const status = repo.status;
    const problems = status?.analysis?.fleet?.problems ?? [];
    const error = repo.error;
    const items = [];
    if (error) items.push({ title: error.code, message: error.message, tone: "bad" });
    problems.forEach((problem) => items.push({
      title: String(problem.kind || "problem").replaceAll("_", " "),
      message: `${problem.message}${problem.prs?.length ? ` · PRs ${problem.prs.map((item) => `#${item}`).join(", ")}` : ""}`,
      tone: problem.kind === "unknown" ? "warn" : "bad",
    }));
    if (!items.length) {
      ui.decisions.innerHTML = empty("No unresolved graph, compatibility, or provider decisions in the latest snapshot.");
      return;
    }
    ui.decisions.innerHTML = items.map((item) => `
      <article class="decision-card">
        <div class="pr-kicker">${badge(item.tone === "bad" ? "Decision" : "Review", item.tone)}</div>
        <h3>${escapeHtml(item.title)}</h3><p>${escapeHtml(item.message)}</p>
      </article>`).join("");
  }

  function render() {
    if (!state?.repositories?.length) return;
    if (!selectedRepo || !state.repositories.some((repo) => repo.id === selectedRepo)) selectedRepo = state.repositories[0].id;
    const repo = state.repositories.find((item) => item.id === selectedRepo);
    ui.loading.hidden = true;
    ui.dashboard.hidden = false;
    renderTabs();
    renderOverview(repo);
    renderCaravans(repo);
    renderWaiting(repo);
    renderDecisions(repo);
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
    }
  }

  async function refreshAll() {
    if (!state) return fetchState();
    ui.refresh.disabled = true;
    try {
      await Promise.all(state.repositories.map((repo) => fetch(`/api/v1/repos/${encodeURIComponent(repo.id)}/refresh`, {
        method: "POST",
        headers: { "X-Cara-CSRF": state.csrf_token, Accept: "application/json" },
      })));
      await fetchState();
      toast("Repository snapshots refreshed");
    } catch (error) {
      toast(error.message);
    } finally {
      ui.refresh.disabled = false;
    }
  }

  ui.refresh.addEventListener("click", refreshAll);
  fetchState();
  setInterval(() => fetchState({ quiet: true }), 5000);
})();
