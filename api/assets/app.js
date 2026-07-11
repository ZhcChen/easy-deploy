(() => {
  const MODAL_CLOSING_CLASS = "is-closing";
  const APP_CONFIG_KIND = "easy-deploy.app-config";
  const COMPOSE_TEMPLATE_KIND = "easy-deploy.compose-template";
  const APP_CONFIG_BASE_FIELDS = [
    "app_key",
    "name",
    "description",
    "environment",
    "work_dir",
    "deploy_strategy",
    "release_source",
    "auto_queue_release",
  ];
  const APP_CONFIG_COMPOSE_FIELDS = ["compose_content", "env_content"];
  const APP_CONFIG_SCRIPT_FIELDS = [
    "deploy_script_pre_deploy",
    "deploy_script_deploy",
    "deploy_script_post_deploy",
    "deploy_script_switch_traffic",
    "deploy_script_cleanup",
  ];
  const APP_CONFIG_SCRIPT_FIELD_ALIASES = {
    deploy_script_pre_deploy: "pre_deploy",
    deploy_script_deploy: "deploy",
    deploy_script_post_deploy: "post_deploy",
    deploy_script_switch_traffic: "switch_traffic",
    deploy_script_cleanup: "cleanup",
  };
  const APP_CONFIG_HEALTH_FIELDS = [
    "health_check_kind",
    "health_endpoint",
    "health_timeout_secs",
    "health_expected_status",
  ];
  const APP_CONFIG_BOOLEAN_FIELDS = ["auto_queue_release"];
  const APP_CONFIG_NUMBER_FIELDS = new Set([
    "health_timeout_secs",
    "health_expected_status",
  ]);
  const APP_CONFIG_FIELD_NAMES = new Set([
    ...APP_CONFIG_BASE_FIELDS,
    ...APP_CONFIG_COMPOSE_FIELDS,
    ...APP_CONFIG_SCRIPT_FIELDS,
    ...APP_CONFIG_HEALTH_FIELDS,
    ...APP_CONFIG_BOOLEAN_FIELDS,
  ]);
  const UTC_TIMESTAMP_TEXT_PATTERN =
    /\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b/;
  const UTC_TIMESTAMP_REPLACE_PATTERN =
    /\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b/g;
  const TIMESTAMP_SKIP_SELECTOR =
    "script, style, textarea, input, select, option, code, pre, kbd, samp";
  const configStatusTimers = new WeakMap();

  const redirectToLoginIfNeeded = (response) => {
    if (!response.redirected) return false;

    let responseUrl;
    try {
      responseUrl = new URL(response.url, window.location.href);
    } catch (_err) {
      return false;
    }
    if (responseUrl.origin !== window.location.origin || responseUrl.pathname !== "/login") {
      return false;
    }

    window.location.replace(`${responseUrl.pathname}${responseUrl.search}${responseUrl.hash}`);
    return true;
  };

  const padTimePart = (value) => String(value).padStart(2, "0");

  const formatEast8Timestamp = (value) => {
    const timestamp = Date.parse(value);
    if (!Number.isFinite(timestamp)) return value;

    const east8 = new Date(timestamp + 8 * 60 * 60 * 1000);
    return [
      east8.getUTCFullYear(),
      padTimePart(east8.getUTCMonth() + 1),
      padTimePart(east8.getUTCDate()),
    ].join("-") + " " + [
      padTimePart(east8.getUTCHours()),
      padTimePart(east8.getUTCMinutes()),
      padTimePart(east8.getUTCSeconds()),
    ].join(":");
  };

  const shouldFormatTimestampNode = (node) => {
    const parent = node.parentElement;
    return parent && !parent.isContentEditable && !parent.closest(TIMESTAMP_SKIP_SELECTOR);
  };

  const formatTimestampTextNode = (node) => {
    const value = node.nodeValue || "";
    if (!UTC_TIMESTAMP_TEXT_PATTERN.test(value) || !shouldFormatTimestampNode(node)) return;

    node.nodeValue = value.replace(UTC_TIMESTAMP_REPLACE_PATTERN, formatEast8Timestamp);
  };

  const formatEast8Timestamps = (root = document.body) => {
    if (!root) return;

    if (root.nodeType === Node.TEXT_NODE) {
      formatTimestampTextNode(root);
      return;
    }

    if (!(root instanceof Element) && root !== document.body) return;
    if (root instanceof Element && root.closest(TIMESTAMP_SKIP_SELECTOR)) return;

    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    const nodes = [];
    while (walker.nextNode()) {
      nodes.push(walker.currentNode);
    }
    nodes.forEach(formatTimestampTextNode);
  };

  const observeEast8Timestamps = () => {
    formatEast8Timestamps();

    const observer = new MutationObserver((mutations) => {
      mutations.forEach((mutation) => {
        mutation.addedNodes.forEach(formatEast8Timestamps);
      });
    });
    observer.observe(document.body, { childList: true, subtree: true });
  };

  const openModal = (button) => {
    const target = button.getAttribute("data-modal-target");
    if (!target) return;

    const dialog = document.getElementById(target);
    if (!(dialog instanceof HTMLDialogElement)) return;

    if (dialog.open) return;
    dialog.classList.remove(MODAL_CLOSING_CLASS);
    dialog.showModal();
  };

  const openHashModal = () => {
    const target = window.location.hash.slice(1);
    if (!target) return;

    const dialog = document.getElementById(target);
    if (dialog instanceof HTMLDialogElement && !dialog.open) {
      dialog.classList.remove(MODAL_CLOSING_CLASS);
      dialog.showModal();
    }
  };

  const closeModal = (dialog) => {
    if (!(dialog instanceof HTMLDialogElement) || !dialog.open) return;

    const prefersReducedMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    if (prefersReducedMotion) {
      dialog.close();
      return;
    }

    dialog.classList.add(MODAL_CLOSING_CLASS);
    window.setTimeout(() => {
      dialog.close();
      dialog.classList.remove(MODAL_CLOSING_CLASS);
    }, 180);
  };

  const textForCopy = (target) => {
    if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
      return target.value;
    }

    return target.textContent || "";
  };

  const fallbackCopy = (target) => {
    if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
      target.focus();
      target.select();
      return document.execCommand("copy");
    }

    const selection = window.getSelection();
    const range = document.createRange();
    range.selectNodeContents(target);
    selection?.removeAllRanges();
    selection?.addRange(range);
    const copied = document.execCommand("copy");
    selection?.removeAllRanges();
    return copied;
  };

  const writeClipboardText = async (value) => {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(value);
      return;
    }

    const textarea = document.createElement("textarea");
    textarea.value = value;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.top = "-1000px";
    textarea.style.left = "-1000px";
    document.body.appendChild(textarea);
    try {
      if (!fallbackCopy(textarea)) {
        throw new Error("copy failed");
      }
    } finally {
      textarea.remove();
    }
  };

  const readClipboardText = async () => {
    if (navigator.clipboard?.readText) {
      return navigator.clipboard.readText();
    }

    return window.prompt("浏览器不允许直接读取剪贴板，请粘贴配置 JSON", "") || "";
  };

  const copyTarget = async (button) => {
    const targetId = button.getAttribute("data-copy-target");
    if (!targetId) return;

    const target = document.getElementById(targetId);
    if (!target) return;

    const originalText = button.textContent;
    const doneText = button.getAttribute("data-copy-done") || "已复制";
    const failText = "复制失败";

    try {
      const value = textForCopy(target).trim();
      if (!value) return;

      await writeClipboardText(value);

      button.textContent = doneText;
    } catch (_err) {
      button.textContent = failText;
    } finally {
      window.setTimeout(() => {
        button.textContent = originalText;
      }, 1400);
    }
  };

  const namedControls = (root, name) =>
    Array.from(root.querySelectorAll("input, textarea, select")).filter(
      (control) => control.name === name,
    );

  const isHiddenControl = (control) =>
    control.hidden ||
    (control instanceof HTMLInputElement && control.type === "hidden");

  const preferredControl = (controls) =>
    controls.find((control) => !isHiddenControl(control)) || controls[0];

  const toBoolean = (value) => {
    if (typeof value === "boolean") return value;
    if (typeof value === "number") return value !== 0;
    return ["1", "true", "on", "yes"].includes(String(value).trim().toLowerCase());
  };

  const normalizeFieldValue = (name, value) => {
    if (APP_CONFIG_BOOLEAN_FIELDS.includes(name)) {
      return toBoolean(value);
    }

    if (APP_CONFIG_NUMBER_FIELDS.has(name)) {
      const trimmed = String(value ?? "").trim();
      if (trimmed === "") return "";
      const parsed = Number(trimmed);
      return Number.isFinite(parsed) ? parsed : trimmed;
    }

    return value ?? "";
  };

  const readFieldValue = (root, name) => {
    const control = preferredControl(namedControls(root, name));
    if (!control) {
      if (name === "app_key") return root.getAttribute("data-app-key") || "";
      if (name === "name") return root.getAttribute("data-app-name") || "";
      if (name === "environment") return root.getAttribute("data-app-environment") || "";
      if (name === "work_dir") return root.getAttribute("data-app-work-dir") || "";
      if (name === "deploy_strategy") {
        return root.getAttribute("data-app-deploy-strategy") || "";
      }
      if (name === "release_source") {
        return root.getAttribute("data-app-release-source") || "";
      }
      if (name === "auto_queue_release") {
        return toBoolean(root.getAttribute("data-app-auto-queue-release") || "");
      }
      return "";
    }

    if (control instanceof HTMLInputElement && control.type === "checkbox") {
      return control.checked;
    }

    return normalizeFieldValue(name, control.value);
  };

  const fieldSnapshot = (root, fields) =>
    fields.reduce((snapshot, name) => {
      snapshot[name] = readFieldValue(root, name);
      return snapshot;
    }, {});

  const collectAppDeployConfig = (root) => ({
    kind: APP_CONFIG_KIND,
    version: 1,
    exported_at: new Date().toISOString(),
    app: {
      id: root.getAttribute("data-app-id") || "",
      name: root.getAttribute("data-app-name") || "",
      key: root.getAttribute("data-app-key") || "",
      type: root.getAttribute("data-app-type") || "",
    },
    config: {
      ...fieldSnapshot(root, APP_CONFIG_BASE_FIELDS),
      ...fieldSnapshot(root, APP_CONFIG_COMPOSE_FIELDS),
      deploy_scripts: {
        pre_deploy: readFieldValue(root, "deploy_script_pre_deploy"),
        deploy: readFieldValue(root, "deploy_script_deploy"),
        post_deploy: readFieldValue(root, "deploy_script_post_deploy"),
        switch_traffic: readFieldValue(root, "deploy_script_switch_traffic"),
        cleanup: readFieldValue(root, "deploy_script_cleanup"),
      },
      health_check: {
        kind: readFieldValue(root, "health_check_kind"),
        endpoint: readFieldValue(root, "health_endpoint"),
        timeout_secs: readFieldValue(root, "health_timeout_secs"),
        expected_status: readFieldValue(root, "health_expected_status"),
      },
      binary: {
        artifact_version: readFieldValue(root, "binary_artifact_version"),
        artifact_path: readFieldValue(root, "binary_artifact_path"),
        exec_args: readFieldValue(root, "binary_exec_args"),
        service_user: readFieldValue(root, "binary_service_user"),
        unit_name: readFieldValue(root, "binary_unit_name"),
        release_strategy: readFieldValue(root, "binary_release_strategy"),
        active_slot: readFieldValue(root, "binary_active_slot"),
        base_port: readFieldValue(root, "binary_base_port"),
        standby_port: readFieldValue(root, "binary_standby_port"),
        proxy_enabled: readFieldValue(root, "binary_proxy_enabled"),
        proxy_kind: readFieldValue(root, "binary_proxy_kind"),
        proxy_domain: readFieldValue(root, "binary_proxy_domain"),
        proxy_config_path: readFieldValue(root, "binary_proxy_config_path"),
      },
    },
  });

  const setConfigTransferStatus = (root, message, tone = "neutral") => {
    const status = root.querySelector("[data-config-transfer-status]");
    if (!status) return;

    const timer = configStatusTimers.get(status);
    if (timer) {
      window.clearTimeout(timer);
    }

    status.textContent = message;
    status.className = `config-transfer-status is-${tone}`;

    if (message) {
      configStatusTimers.set(
        status,
        window.setTimeout(() => {
          status.textContent = "";
          status.className = "config-transfer-status";
        }, 3600),
      );
    }
  };

  const isAppConfigControl = (target) =>
    (target instanceof HTMLInputElement ||
      target instanceof HTMLTextAreaElement ||
      target instanceof HTMLSelectElement) &&
    APP_CONFIG_FIELD_NAMES.has(target.name);

  const setControlValue = (control, value) => {
    if (control instanceof HTMLInputElement && control.type === "checkbox") {
      control.checked = toBoolean(value);
      return;
    }

    if (APP_CONFIG_BOOLEAN_FIELDS.includes(control.name)) {
      control.value = toBoolean(value) ? "true" : "false";
      return;
    }

    control.value = String(value ?? "");
  };

  const controlExportValue = (control) => {
    if (control instanceof HTMLInputElement && control.type === "checkbox") {
      return control.checked;
    }

    return normalizeFieldValue(control.name, control.value);
  };

  const syncAppConfigControl = (control) => {
    const root = control.closest("[data-app-config-transfer]");
    if (!root) return;

    const value = controlExportValue(control);
    namedControls(root, control.name).forEach((target) => {
      if (target !== control) {
        setControlValue(target, value);
      }
    });

  };

  const assignIfPresent = (target, key, value) => {
    if (value !== undefined && value !== null) {
      target[key] = value;
    }
  };

  const unquoteYamlScalar = (value) => {
    const trimmed = String(value ?? "").trim();
    if (
      (trimmed.startsWith('"') && trimmed.endsWith('"')) ||
      (trimmed.startsWith("'") && trimmed.endsWith("'"))
    ) {
      return trimmed.slice(1, -1);
    }
    return trimmed;
  };

  const parseTemplateAppYaml = (content) => {
    if (typeof content !== "string" || !content.trim()) return {};
    return content.split(/\r?\n/).reduce((result, line) => {
      if (!line || /^\s/.test(line) || line.trimStart().startsWith("#")) return result;
      const match = line.match(/^([A-Za-z0-9_]+)\s*:\s*(.*)$/);
      if (!match) return result;
      result[match[1]] = unquoteYamlScalar(match[2]);
      return result;
    }, {});
  };

  const firstDefined = (...values) =>
    values.find((value) => value !== undefined && value !== null);

  const normalizeScriptKey = (key) =>
    String(key || "")
      .trim()
      .toLowerCase()
      .replace(/\.(sh|bash)$/, "")
      .replace(/[^a-z0-9]+/g, "_")
      .replace(/^_+|_+$/g, "");

  const scriptBlock = (name, content) =>
    `\n# ${name}\n${String(content ?? "").trim()}\n`;

  const normalizeTemplateScripts = (scripts) => {
    if (!scripts || typeof scripts !== "object" || Array.isArray(scripts)) return {};
    const normalized = {};
    const extra = [];

    Object.entries(scripts).forEach(([name, content]) => {
      const script = String(content ?? "");
      if (!script.trim()) return;
      const key = normalizeScriptKey(name);
      if (key === "pre_deploy") normalized.pre_deploy = script;
      else if (key === "deploy") normalized.deploy = script;
      else if (key === "post_deploy") normalized.post_deploy = script;
      else if (key === "switch_traffic") normalized.switch_traffic = script;
      else if (key === "cleanup") normalized.cleanup = script;
      else extra.push([name, script]);
    });

    if (extra.length > 0 && !normalized.post_deploy) {
      normalized.post_deploy = extra
        .sort(([left], [right]) => String(left).localeCompare(String(right)))
        .map(([name, content]) => scriptBlock(name, content))
        .join("")
        .trimStart();
    }

    return normalized;
  };

  const normalizeImportedAppConfig = (payload) => {
    if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
      throw new Error("invalid config payload");
    }

    if (payload.kind && ![APP_CONFIG_KIND, COMPOSE_TEMPLATE_KIND].includes(payload.kind)) {
      throw new Error("config kind mismatch");
    }

    if (payload.schema) {
      throw new Error("config schema mismatch");
    }

    const source =
      payload.config && typeof payload.config === "object" ? payload.config : payload;
    const appYaml = parseTemplateAppYaml(
      firstDefined(
        source.app_yaml,
        source.appYaml,
        source["app.yaml"],
        source["app.yaml.example"],
      ),
    );
    const health =
      source.health_check && typeof source.health_check === "object"
        ? source.health_check
        : {};
    const deployScripts =
      source.deploy_scripts && typeof source.deploy_scripts === "object"
        ? source.deploy_scripts
        : normalizeTemplateScripts(source.scripts);
    const config = {};

    [...APP_CONFIG_BASE_FIELDS, ...APP_CONFIG_COMPOSE_FIELDS].forEach((key) => {
      assignIfPresent(config, key, firstDefined(source[key], appYaml[key]));
    });
    assignIfPresent(
      config,
      "compose_content",
      firstDefined(
        source.compose_content,
        source.compose_yaml,
        source.composeYaml,
        source.compose,
        source["compose.yaml"],
        source["compose.yaml.example"],
      ),
    );
    assignIfPresent(
      config,
      "env_content",
      firstDefined(
        source.env_content,
        source.env,
        source.env_file,
        source.envFile,
        source[".env"],
        source[".env.example"],
      ),
    );
    APP_CONFIG_SCRIPT_FIELDS.forEach((key) => {
      assignIfPresent(config, key, source[key] ?? deployScripts[APP_CONFIG_SCRIPT_FIELD_ALIASES[key]]);
    });

    assignIfPresent(config, "health_check_kind", firstDefined(source.health_check_kind, health.kind, appYaml.health_check_kind));
    assignIfPresent(config, "health_endpoint", firstDefined(source.health_endpoint, health.endpoint, appYaml.health_endpoint));
    assignIfPresent(
      config,
      "health_timeout_secs",
      firstDefined(source.health_timeout_secs, health.timeout_secs, appYaml.health_timeout_secs),
    );
    assignIfPresent(
      config,
      "health_expected_status",
      firstDefined(source.health_expected_status, health.expected_status, appYaml.health_expected_status),
    );

    return config;
  };

  const applyAppDeployConfig = (root, config) => {
    let applied = 0;
    Object.entries(config).forEach(([name, value]) => {
      if (!APP_CONFIG_FIELD_NAMES.has(name)) return;

      const controls = namedControls(root, name);
      if (controls.length === 0) return;

      const normalized = normalizeFieldValue(name, value);
      controls.forEach((control) => {
        setControlValue(control, normalized);
        control.dispatchEvent(new Event("input", { bubbles: true }));
        control.dispatchEvent(new Event("change", { bubbles: true }));
      });
      applied += 1;
    });

    return applied;
  };

  const setButtonTextTemporarily = (button, text) => {
    const originalText = button.textContent;
    button.textContent = text;
    window.setTimeout(() => {
      button.textContent = originalText;
    }, 1400);
  };

  const handleAppConfigExport = async (button) => {
    const root = button.closest("[data-app-config-transfer]");
    if (!root) return;

    if (button instanceof HTMLButtonElement) {
      button.disabled = true;
    }

    try {
      const configPackage = collectAppDeployConfig(root);
      await writeClipboardText(JSON.stringify(configPackage, null, 2));
      setButtonTextTemporarily(button, "已复制");
      setConfigTransferStatus(root, "配置已复制到剪贴板", "success");
    } catch (_err) {
      setButtonTextTemporarily(button, "复制失败");
      setConfigTransferStatus(root, "复制失败，请检查浏览器剪贴板权限", "error");
    } finally {
      if (button instanceof HTMLButtonElement) {
        button.disabled = false;
      }
    }
  };

  const handleAppConfigImport = async (button) => {
    const root = button.closest("[data-app-config-transfer]");
    if (!root) return;

    if (button instanceof HTMLButtonElement) {
      button.disabled = true;
    }

    try {
      const text = (await readClipboardText()).trim();
      if (!text) {
        setConfigTransferStatus(root, "剪贴板没有可导入内容", "error");
        return;
      }

      const payload = JSON.parse(text);
      const config = normalizeImportedAppConfig(payload);
      const applied = applyAppDeployConfig(root, config);
      if (applied === 0) {
        setConfigTransferStatus(root, "没有找到可导入的部署配置", "error");
        return;
      }

      setButtonTextTemporarily(button, "已导入");
      setConfigTransferStatus(root, "已导入到页面，确认后保存", "success");
    } catch (_err) {
      setButtonTextTemporarily(button, "导入失败");
      setConfigTransferStatus(root, "导入失败，剪贴板内容不是有效配置", "error");
    } finally {
      if (button instanceof HTMLButtonElement) {
        button.disabled = false;
      }
    }
  };

  const setNodeStatusCell = (cell, status, tone, dockerStatus, message) => {
    cell.replaceChildren();

    const badge = document.createElement("span");
    badge.className = `badge tone-${tone}`;
    badge.textContent = status;
    cell.appendChild(badge);

    const dockerLine = document.createElement("span");
    dockerLine.setAttribute("data-node-docker-status", "");
    dockerLine.textContent = `Docker: ${dockerStatus}`;
    cell.appendChild(dockerLine);

    if (message) {
      const messageLine = document.createElement("span");
      messageLine.className = "node-status-message";
      messageLine.textContent = message;
      cell.appendChild(messageLine);
    }
  };

  const setNodeStatusLoading = (cell) => {
    cell.replaceChildren();

    const badge = document.createElement("span");
    badge.className = "badge tone-active node-status-loading";
    badge.textContent = "探测中";
    cell.appendChild(badge);

    const hint = document.createElement("span");
    hint.textContent = "正在刷新节点状态";
    cell.appendChild(hint);
  };

  const handleNodeCheckSubmit = async (form) => {
    const row = form.closest("[data-node-row]");
    const statusCell = row?.querySelector("[data-node-status-cell]");
    const button = form.querySelector('button[type="submit"]');
    if (!row || !statusCell || !(button instanceof HTMLButtonElement)) {
      form.submit();
      return;
    }

    const originalStatusHtml = statusCell.innerHTML;
    const originalButtonText = button.textContent;
    button.disabled = true;
    button.textContent = "探测中";
    setNodeStatusLoading(statusCell);

    try {
      const response = await fetch(form.action, {
        method: "POST",
        body: new URLSearchParams(new FormData(form)),
        headers: {
          Accept: "application/json",
          "Content-Type": "application/x-www-form-urlencoded",
          "X-Requested-With": "easy-deploy-node-check",
        },
      });
      if (redirectToLoginIfNeeded(response)) return;

      if (!response.ok) {
        throw new Error((await response.text()).trim() || "探测失败");
      }

      const result = await response.json();
      setNodeStatusCell(
        statusCell,
        result.status || "未探测",
        result.status_tone || "neutral",
        result.docker_status || "unknown",
        result.message || "",
      );
    } catch (err) {
      statusCell.innerHTML = originalStatusHtml;
      const errorLine = document.createElement("span");
      errorLine.className = "node-status-error";
      errorLine.textContent = err instanceof Error ? err.message : "探测失败";
      statusCell.appendChild(errorLine);
    } finally {
      button.disabled = false;
      button.textContent = originalButtonText;
    }
  };

  const searchableSelectOptions = (select) =>
    Array.from(select.querySelectorAll("[data-searchable-select-option]"));

  const selectedSearchableLabels = (select) =>
    Array.from(select.querySelectorAll('input[type="checkbox"]:checked'))
      .map((input) =>
        input
          .closest("[data-searchable-select-option]")
          ?.querySelector("strong")
          ?.textContent?.trim(),
      )
      .filter(Boolean);

  const searchableText = (value) => (value || "").trim().toLowerCase();

  const updateSearchableSelect = (select) => {
    const labels = selectedSearchableLabels(select);
    const value = select.querySelector("[data-searchable-select-value]");
    const count = select
      .closest(".form-section")
      ?.querySelector("[data-node-select-count]");

    if (value) {
      if (labels.length === 0) {
        value.textContent = "请选择目标节点";
      } else if (labels.length <= 2) {
        value.textContent = labels.join("、");
      } else {
        value.textContent = `${labels.slice(0, 2).join("、")} 等 ${labels.length} 个节点`;
      }
    }

    if (count) {
      count.textContent = labels.length === 0 ? "未选择" : `已选择 ${labels.length} 个`;
    }
  };

  const filterSearchableSelect = (select) => {
    const input = select.querySelector("[data-searchable-select-input]");
    const empty = select.querySelector("[data-searchable-select-empty]");
    const query = searchableText(input?.value);
    let visibleCount = 0;

    searchableSelectOptions(select).forEach((option) => {
      const content = searchableText(
        option.getAttribute("data-search-text") || option.textContent,
      );
      const visible = query === "" || content.includes(query);
      option.hidden = !visible;
      if (visible) visibleCount += 1;
    });

    if (empty) {
      empty.hidden = visibleCount > 0;
    }
  };

  const closeOtherSearchableSelects = (current) => {
    document
      .querySelectorAll("[data-searchable-select][open]")
      .forEach((select) => {
        if (select !== current && select instanceof HTMLDetailsElement) {
          select.open = false;
        }
      });
  };

  const initSearchableSelects = () => {
    document.querySelectorAll("[data-searchable-select]").forEach((select) => {
      filterSearchableSelect(select);
      updateSearchableSelect(select);
    });
  };

  const getPathValue = (source, path) =>
    path.split(".").reduce((value, key) => {
      if (value && typeof value === "object" && key in value) {
        return value[key];
      }
      return undefined;
    }, source);

  const setHostMetricText = (panel, key, value) => {
    panel.querySelectorAll(`[data-host-metric="${key}"]`).forEach((element) => {
      element.textContent = value == null || value === "" ? "--" : String(value);
    });
  };

  const setHostMetricBar = (panel, key, value) => {
    const numeric = Number(value);
    const width = Number.isFinite(numeric)
      ? Math.max(0, Math.min(100, numeric))
      : 0;
    panel.querySelectorAll(`[data-host-metric-bar="${key}"]`).forEach((element) => {
      element.style.width = `${width}%`;
    });
  };

  const appendTextElement = (parent, tagName, className, text) => {
    const element = document.createElement(tagName);
    if (className) {
      element.className = className;
    }
    element.textContent = text == null || text === "" ? "--" : String(text);
    parent.appendChild(element);
    return element;
  };

  const renderDiskRateDevices = (panel, devices) => {
    const container = panel.querySelector("[data-disk-rate-devices]");
    if (!container) return;

    container.replaceChildren();
    if (!Array.isArray(devices) || devices.length === 0) {
      const empty = document.createElement("div");
      empty.className = "modal-empty-state";
      appendTextElement(empty, "span", "", "暂无磁盘速率数据");
      appendTextElement(empty, "p", "", "等待下一次资源采样，或当前系统不支持磁盘速率采集。");
      container.appendChild(empty);
      return;
    }

    devices.slice(0, 20).forEach((device, index) => {
      const row = document.createElement("article");
      row.className = "disk-rate-device-row";

      appendTextElement(row, "span", "disk-rate-rank", `#${index + 1}`);

      const nameCell = document.createElement("div");
      nameCell.className = "disk-rate-device-name";
      appendTextElement(nameCell, "strong", "", device.name);
      appendTextElement(nameCell, "span", "", `总 ${device.total_label || "--"}`);
      row.appendChild(nameCell);

      const readCell = document.createElement("div");
      readCell.className = "disk-rate-device-value";
      appendTextElement(readCell, "span", "", "读");
      appendTextElement(readCell, "strong", "", device.read_label);
      row.appendChild(readCell);

      const writeCell = document.createElement("div");
      writeCell.className = "disk-rate-device-value";
      appendTextElement(writeCell, "span", "", "写");
      appendTextElement(writeCell, "strong", "", device.write_label);
      row.appendChild(writeCell);

      const busyCell = document.createElement("div");
      busyCell.className = "disk-rate-device-busy";
      const busyHeader = document.createElement("div");
      appendTextElement(busyHeader, "span", "", "忙碌率");
      appendTextElement(busyHeader, "strong", "", device.utilization_label);
      busyCell.appendChild(busyHeader);
      const bar = document.createElement("div");
      bar.className = "metric-bar mini-metric-bar";
      const fill = document.createElement("span");
      const busyValue = Number(device.utilization_percent);
      fill.style.width = `${Number.isFinite(busyValue) ? Math.max(0, Math.min(100, busyValue)) : 0}%`;
      bar.appendChild(fill);
      busyCell.appendChild(bar);
      row.appendChild(busyCell);

      container.appendChild(row);
    });
  };

  const renderDiskRateProcesses = (panel, processes) => {
    const container = panel.querySelector("[data-disk-rate-processes]");
    if (!container) return;

    container.replaceChildren();
    if (!Array.isArray(processes) || processes.length === 0) {
      const empty = document.createElement("div");
      empty.className = "modal-empty-state";
      appendTextElement(empty, "span", "", "暂无进程 IO 数据");
      appendTextElement(empty, "p", "", "等待下一次采样；如果一直为空，通常是当前服务账号无权限读取其他进程。");
      container.appendChild(empty);
      return;
    }

    processes.slice(0, 20).forEach((process, index) => {
      const row = document.createElement("article");
      row.className = "disk-rate-process-row";

      appendTextElement(row, "span", "disk-rate-rank", `#${index + 1}`);

      const processCell = document.createElement("div");
      processCell.className = "disk-rate-process-name";
      appendTextElement(processCell, "strong", "", process.name);
      const meta = document.createElement("span");
      meta.textContent = process.container_id
        ? `PID ${process.pid} · 容器 ${process.container_id}`
        : `PID ${process.pid}`;
      processCell.appendChild(meta);
      row.appendChild(processCell);

      const readCell = document.createElement("div");
      readCell.className = "disk-rate-device-value";
      appendTextElement(readCell, "span", "", "读");
      appendTextElement(readCell, "strong", "", process.read_label);
      row.appendChild(readCell);

      const writeCell = document.createElement("div");
      writeCell.className = "disk-rate-device-value";
      appendTextElement(writeCell, "span", "", "写");
      appendTextElement(writeCell, "strong", "", process.write_label);
      row.appendChild(writeCell);

      const totalCell = document.createElement("div");
      totalCell.className = "disk-rate-device-value";
      appendTextElement(totalCell, "span", "", "总");
      appendTextElement(totalCell, "strong", "", process.total_label);
      row.appendChild(totalCell);

      container.appendChild(row);
    });
  };

  const updateHostMetricsPanel = (panel, payload) => {
    const metricKeys = [
      "cpu.percent_label",
      "cpu.detail",
      "memory.percent_label",
      "memory.detail",
      "disk.percent_label",
      "disk.detail",
      "disk.mount_point",
      "disk_rate.detail",
      "disk_rate.utilization_label",
      "disk_rate.process_detail",
      "network_rate.detail",
    ];

    metricKeys.forEach((key) => {
      setHostMetricText(panel, key, getPathValue(payload, key));
    });
    [
      "cpu.percent",
      "memory.percent",
      "disk.percent",
      "disk_rate.utilization_percent",
    ].forEach((key) => {
      setHostMetricBar(panel, key, getPathValue(payload, key));
    });
    renderDiskRateDevices(panel, getPathValue(payload, "disk_rate.devices"));
    renderDiskRateProcesses(panel, getPathValue(payload, "disk_rate.processes"));

    const status = panel.querySelector("[data-host-metrics-status]");
    if (status) {
      const sampledAt = Number(payload.sampled_at_epoch_ms);
      const time = Number.isFinite(sampledAt)
        ? new Date(sampledAt).toLocaleTimeString("zh-CN", { hour12: false })
        : new Date().toLocaleTimeString("zh-CN", { hour12: false });
      status.textContent = `已刷新 ${time}`;
    }
  };

  const initHostMetrics = () => {
    const panel = document.querySelector("[data-host-metrics]");
    if (!panel) return;

    const intervalSelect = panel.querySelector("[data-host-metrics-interval]");
    const status = panel.querySelector("[data-host-metrics-status]");
    let timer = null;
    let loading = false;

    const refresh = async () => {
      if (loading) return;
      loading = true;
      try {
        const response = await fetch("/api/dashboard/host-metrics", {
          credentials: "same-origin",
          headers: { Accept: "application/json" },
        });
        if (redirectToLoginIfNeeded(response)) return;
        if (!response.ok) {
          throw new Error(`status ${response.status}`);
        }
        const payload = await response.json();
        updateHostMetricsPanel(panel, payload);
      } catch (_err) {
        if (status) {
          status.textContent = "刷新失败";
        }
      } finally {
        loading = false;
      }
    };

    const intervalMs = () => {
      const selected = Number(intervalSelect?.value || 1000);
      return [1000, 3000, 5000, 10000].includes(selected) ? selected : 1000;
    };

    const restart = () => {
      if (timer !== null) {
        window.clearInterval(timer);
      }
      refresh();
      timer = window.setInterval(refresh, intervalMs());
    };

    intervalSelect?.addEventListener("change", restart);
    restart();
  };

  document.addEventListener("click", (event) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target) return;

    const copyButton = target.closest("[data-copy-target]");
    if (copyButton) {
      event.preventDefault();
      copyTarget(copyButton);
      return;
    }

    const exportButton = target.closest("[data-app-config-export]");
    if (exportButton) {
      event.preventDefault();
      handleAppConfigExport(exportButton);
      return;
    }

    const importButton = target.closest("[data-app-config-import]");
    if (importButton) {
      event.preventDefault();
      handleAppConfigImport(importButton);
      return;
    }

    const openButton = target.closest("[data-modal-target]");
    if (openButton) {
      event.preventDefault();
      openModal(openButton);
      return;
    }

    const closeButton = target.closest("[data-modal-close]");
    if (closeButton) {
      event.preventDefault();
      const dialog = closeButton.closest("dialog");
      if (dialog instanceof HTMLDialogElement) {
        closeModal(dialog);
      }
      return;
    }

    closeOtherSearchableSelects(target.closest("[data-searchable-select]"));
  });

  document.addEventListener("input", (event) => {
    const input = event.target;
    if (isAppConfigControl(input)) {
      syncAppConfigControl(input);
    }

    if (!(input instanceof HTMLInputElement)) return;
    if (!input.matches("[data-searchable-select-input]")) return;

    const select = input.closest("[data-searchable-select]");
    if (!select) return;

    filterSearchableSelect(select);
  });

  document.addEventListener("change", (event) => {
    const input = event.target;
    if (isAppConfigControl(input)) {
      syncAppConfigControl(input);
    }
  });

  document.addEventListener("submit", (event) => {
    const form = event.target;
    if (!(form instanceof HTMLFormElement)) return;
    if (!form.matches("[data-node-check-form]")) return;

    event.preventDefault();
    handleNodeCheckSubmit(form);
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      const target = event.target instanceof Element ? event.target : null;
      const openButton = target?.closest('[data-modal-target][role="button"]');
      if (openButton) {
        event.preventDefault();
        openModal(openButton);
        return;
      }
    }

    if (event.key !== "Escape") return;

    const openSelect = document.querySelector("[data-searchable-select][open]");
    if (openSelect instanceof HTMLDetailsElement) {
      openSelect.open = false;
      event.preventDefault();
      return;
    }

    const modal = document.querySelector("dialog.modal-dialog[open]");
    if (modal instanceof HTMLDialogElement) {
      event.preventDefault();
      return;
    }

  });

  document.addEventListener(
    "cancel",
    (event) => {
      if (event.target instanceof HTMLDialogElement) {
        event.preventDefault();
      }
    },
    true,
  );

  document.addEventListener(
    "toggle",
    (event) => {
      const select = event.target;
      if (!(select instanceof HTMLDetailsElement)) return;
      if (!select.matches("[data-searchable-select]")) return;

      if (select.open) {
        closeOtherSearchableSelects(select);
        filterSearchableSelect(select);
        window.setTimeout(() => {
          select.querySelector("[data-searchable-select-input]")?.focus();
        }, 0);
      }
    },
    true,
  );

  document.addEventListener("focusin", (event) => {
    if (event.target.matches(".copy-field textarea[readonly]")) {
      event.target.select();
    }
  });

  const loadPermissionDependencies = () => {
    const element = document.getElementById("permission-dependencies");
    if (!element) return {};

    try {
      const parsed = JSON.parse(element.textContent || "{}");
      return parsed && typeof parsed === "object" ? parsed : {};
    } catch (_err) {
      return {};
    }
  };

  const permissionDependencies = loadPermissionDependencies();

  document.addEventListener("change", (event) => {
    const checkbox = event.target;
    if (!(checkbox instanceof HTMLInputElement)) return;
    if (checkbox.type !== "checkbox") return;

    const select = checkbox.closest("[data-searchable-select]");
    if (!select) return;

    updateSearchableSelect(select);
  });

  document.addEventListener("change", (event) => {
    const checkbox = event.target;
    if (!(checkbox instanceof HTMLInputElement)) return;
    if (checkbox.type !== "checkbox" || checkbox.name !== "permission_ids") return;
    if (!checkbox.checked) return;

    const permissionKey = checkbox.getAttribute("data-permission-key");
    const dependencies = permissionDependencies[permissionKey] || [];
    if (!Array.isArray(dependencies) || dependencies.length === 0) return;

    const form = checkbox.closest("form");
    if (!form) return;

    dependencies.forEach((dependencyKey) => {
      const dependency = form.querySelector(
        `input[name="permission_ids"][data-permission-key="${CSS.escape(dependencyKey)}"]`,
      );
      if (dependency instanceof HTMLInputElement) {
        dependency.checked = true;
      }
    });
  });

  window.addEventListener("hashchange", openHashModal);
  observeEast8Timestamps();
  initSearchableSelects();
  initHostMetrics();
  openHashModal();
})();
