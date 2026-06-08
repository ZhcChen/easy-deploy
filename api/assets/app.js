(() => {
  const MODAL_CLOSING_CLASS = "is-closing";

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

      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(value);
      } else if (!fallbackCopy(target)) {
        throw new Error("copy failed");
      }

      button.textContent = doneText;
    } catch (_err) {
      button.textContent = failText;
    } finally {
      window.setTimeout(() => {
        button.textContent = originalText;
      }, 1400);
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

  document.addEventListener("click", (event) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target) return;

    const copyButton = target.closest("[data-copy-target]");
    if (copyButton) {
      event.preventDefault();
      copyTarget(copyButton);
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
    if (!(input instanceof HTMLInputElement)) return;
    if (!input.matches("[data-searchable-select-input]")) return;

    const select = input.closest("[data-searchable-select]");
    if (!select) return;

    filterSearchableSelect(select);
  });

  document.addEventListener("submit", (event) => {
    const form = event.target;
    if (!(form instanceof HTMLFormElement)) return;
    if (!form.matches("[data-node-check-form]")) return;

    event.preventDefault();
    handleNodeCheckSubmit(form);
  });

  document.addEventListener("keydown", (event) => {
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
  initSearchableSelects();
  openHashModal();
})();
