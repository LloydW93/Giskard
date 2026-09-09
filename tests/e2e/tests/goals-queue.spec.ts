import { test, expect, type Page } from "@playwright/test";
import { login, SCRIPTED_REPLY } from "./helpers";

async function thread(page: Page) {
  await login(page);
  await page.locator(".proj", { hasText:"Demo" }).locator(".project-add").click();
  await page.locator("#input").fill("Prepare goals and queue tests.");
  await page.locator("#sendBtn").click();
  await expect(page.locator(".msg.agent", { hasText:SCRIPTED_REPLY })).toBeVisible();
  await expect(page.locator("#stopBtn")).toBeHidden();
}

async function enable(page: Page) {
  await page.evaluate(() => {
    const selected = JSON.parse(localStorage.getItem("giskard.lastThread")!);
    (window as any).handleServer({ type:"thread_capabilities", thread_id:selected.tid, goals_queue:true }, {});
  });
}

// Real replay-harness state survives closing the panel and refreshing the browser.
test("goal and queue actions use native state and survive refresh", async ({ page }) => {
  await thread(page);
  await page.locator("#goalsQueueBtn").click();
  await expect(page.locator("#goalsQueueSettingsNote")).toContainText("selected model, service tier, mode and permissions");
  await expect(page.locator("#goalsQueueSettingsNote")).toContainText("Work already running keeps its settings");
  await expect(page.locator("#goalSummary")).toHaveText("No goal set.");
  await page.locator("#goalObjective").fill("Complete the queue review");
  await page.locator("#goalBudget").fill("5000");
  await page.locator("#goalSave").click();
  await expect(page.locator("#goalSummary")).toContainText("Complete the queue review");
  for (const text of ["First queued task", "Second queued task"]) {
    await page.locator("#queueText").fill(text);
    await page.locator("#queueSave").click();
    await expect(page.locator("#queueText")).toHaveValue("");
  }
  let rows = page.locator("#goalQueueList li");
  await expect(rows).toHaveCount(2);
  await rows.nth(1).getByRole("button", { name:"Move up", exact:true }).click();
  await expect(rows.first()).toContainText("Second queued task");
  await rows.first().getByRole("button", { name:"Edit", exact:true }).click();
  await page.locator("#queueText").fill("Edited queued task");
  await page.locator("#queueSave").click();
  await expect(rows.first()).toContainText("Edited queued task");
  await page.reload();
  await page.locator("#goalsQueueBtn").click();
  await expect(page.locator("#goalSummary")).toContainText("Complete the queue review");
  rows = page.locator("#goalQueueList li");
  await expect(rows.first()).toContainText("Edited queued task");
  await rows.nth(1).getByRole("button", { name:"Delete", exact:true }).click();
  await expect(rows).toHaveCount(1);
  await page.locator("#goalStatus").selectOption("complete");
  await page.locator("#goalSave").click();
  await expect(page.locator("#goalSummary")).toContainText("complete");
  await page.locator("#goalClear").click();
  await expect(page.locator("#goalSummary")).toHaveText("No goal set.");
  await page.locator("#queueStart").click();
  await expect(rows).toHaveCount(0);
  await page.locator("#goalsQueueClose").click();
  await expect(page.locator(".msg.user", { hasText:"Edited queued task" })).toBeVisible();
});

test("queue failures preserve drafts and never retry mutations automatically", async ({ page }) => {
  await thread(page);
  await enable(page);
  let posts = 0;
  await page.route("**/goals-queue", async route => {
    if (route.request().method() === "POST") {
      ++posts;
      await route.fulfill({ status:409, contentType:"application/json", body:JSON.stringify({ message:"Queue changed in another client" }) });
    } else await route.fulfill({ json:{ goal:null, queue:[], next_cursor:null } });
  });
  await page.locator("#goalsQueueBtn").click();
  await page.locator("#queueText").fill("Keep my pending prompt");
  await page.locator("#queueSave").click();
  await expect(page.locator("#goalsQueueNotice")).toContainText("Queue changed in another client");
  await expect(page.locator("#queueText")).toHaveValue("Keep my pending prompt");
  await page.locator("#goalsQueueRefresh").click();
  await expect(page.locator("#queueSave")).toBeEnabled();
  expect(posts).toBe(1);
});

test("read-only child goals can be inspected but not mutated", async ({ page }) => {
  await thread(page);
  await enable(page);
  await page.route("**/goals-queue", route => route.fulfill({ json:{ goal:null, queue:[{ id:"one", text:"Queued child task", has_other_input:false }], next_cursor:null } }));
  await page.evaluate(() => { (window as any).managedThreadReadOnly = () => true; (window as any).updateComposerControls(); });
  await page.locator("#goalsQueueBtn").click();
  await expect(page.locator("#goalQueueList")).toContainText("Queued child task");
  await expect(page.locator("#goalsQueueReadOnly")).toContainText("read-only");
  await expect(page.locator("#goalSave")).toBeDisabled();
  await expect(page.locator("#queueSave")).toBeDisabled();
  await expect(page.locator("#queueStart")).toBeDisabled();
  await expect(page.getByRole("button", { name:"Delete", exact:true })).toBeDisabled();
});

test("pagination preserves other input and disables incomplete reorder", async ({ page }) => {
  await thread(page);
  await enable(page);
  await page.route("**/goals-queue*", route => route.fulfill({ json:route.request().url().includes("cursor=")
    ? { goal:null, queue:[{ id:"two", text:"Second page", has_other_input:false }], next_cursor:null }
    : { goal:null, queue:[{ id:"one", text:"Prompt with image", has_other_input:true }], next_cursor:"page-2" }
  }));
  await page.locator("#goalsQueueBtn").click();
  const rows = page.locator("#goalQueueList li");
  await expect(rows.first()).toContainText("Text replacement is unavailable");
  await expect(rows.first().getByRole("button", { name:"Edit", exact:true })).toHaveCount(0);
  await expect(rows.first().getByRole("button", { name:"Move down", exact:true })).toBeDisabled();
  await page.locator("#goalQueueMore").click();
  await expect(rows).toHaveCount(2);
  await expect(rows.first().getByRole("button", { name:"Move down", exact:true })).toBeEnabled();
  await expect(page.locator("#goalQueueMore")).toBeHidden();
});

test("native updates refresh projection without replacing an edited objective", async ({ page }) => {
  await thread(page);
  await enable(page);
  let objective = "Original goal";
  await page.route("**/goals-queue", route => route.fulfill({ json:{ goal:{ objective, status:"active", tokens_used:20, token_budget:1000 }, queue:[], next_cursor:null } }));
  await page.locator("#goalsQueueBtn").click();
  await expect(page.locator("#goalObjective")).toHaveValue("Original goal");
  await page.locator("#goalObjective").fill("My unsaved goal");
  objective = "Changed by agent";
  await page.evaluate(() => {
    const selected = JSON.parse(localStorage.getItem("giskard.lastThread")!);
    (window as any).handleServer({ type:"event", thread_id:selected.tid, agent_event:{ kind:"goals_queue_changed", thread:selected.tid } }, {});
  });
  await expect(page.locator("#goalSummary")).toContainText("Changed by agent");
  await expect(page.locator("#goalObjective")).toHaveValue("My unsaved goal");
});

test("closing and reopening discards stale reads and traps keyboard focus", async ({ page }) => {
  await thread(page);
  await enable(page);
  let calls = 0;
  let release!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  await page.route("**/goals-queue", async route => {
    const call = ++calls;
    if (call === 1) await pending;
    await route.fulfill({ json:{ goal:{ objective:call === 1 ? "Stale goal" : "Current goal", status:"active" }, queue:[], next_cursor:null } });
  });
  await page.locator("#goalsQueueBtn").click();
  await expect.poll(() => calls).toBe(1);
  await page.locator("#goalsQueueClose").click();
  await page.locator("#goalsQueueBtn").click();
  await expect(page.locator("#goalSummary")).toContainText("Current goal");
  release();
  await page.locator("#queueSave").focus();
  await page.keyboard.press("Tab");
  await expect(page.locator("#goalsQueueClose")).toBeFocused();
  await expect(page.locator("#goalSummary")).toContainText("Current goal");
  await page.keyboard.press("Escape");
  await expect(page.locator("#goalsQueueOverlay")).not.toHaveClass(/open/);
  await expect(page.locator("#goalsQueueBtn")).toBeFocused();
});

test("status and budget edits omit unchanged objective and preserve completed goal usage", async ({ page }) => {
  await thread(page);
  await enable(page);
  const commands: Record<string, unknown>[] = [];
  let goal = { objective:"Completed review", status:"complete", tokens_used:2400, token_budget:5000, time_used_seconds:90 };
  await page.route("**/goals-queue", async route => {
    if (route.request().method() === "POST") {
      const command = route.request().postDataJSON();
      commands.push(command);
      // Native terminal goals reset accounting whenever an objective is explicitly supplied.
      if (Object.hasOwn(command, "objective")) goal = { ...goal, objective:command.objective, tokens_used:0, time_used_seconds:0 };
      goal = { ...goal, status:command.status, token_budget:command.token_budget ?? goal.token_budget };
    }
    await route.fulfill({ json:{ goal, queue:[], next_cursor:null } });
  });
  await page.locator("#goalsQueueBtn").click();
  await expect(page.locator("#goalObjective")).toHaveValue("Completed review");
  await page.locator("#goalBudget").fill("6000");
  await page.locator("#goalSave").click();
  await expect(page.locator("#goalSummary")).toContainText("2,400 tokens used of 6,000");
  expect(commands[0]).toEqual({ action:"set_goal", status:"complete", token_budget:6000 });
  await page.locator("#goalStatus").selectOption("paused");
  await page.locator("#goalSave").click();
  await expect(page.locator("#goalSummary")).toContainText("paused");
  expect(commands[1]).toEqual({ action:"set_goal", status:"paused", token_budget:6000 });
  await expect(page.locator("#goalSummary")).toContainText("90 seconds");
  await page.locator("#goalObjective").fill("A new review");
  await page.locator("#goalSave").click();
  await expect(page.locator("#goalSummary")).toContainText("A new review");
  expect(commands[2].objective).toBe("A new review");
});

test("status-only edits do not overwrite an objective changed by a native update", async ({ page }) => {
  await thread(page);
  await enable(page);
  const commands: Record<string, unknown>[] = [];
  let goal = { objective:"Original objective", status:"complete", tokens_used:1200 };
  await page.route("**/goals-queue", async route => {
    if (route.request().method() === "POST") {
      const command = route.request().postDataJSON();
      commands.push(command);
      goal = { ...goal, ...command };
    }
    await route.fulfill({ json:{ goal, queue:[], next_cursor:null } });
  });
  await page.locator("#goalsQueueBtn").click();
  await expect(page.locator("#goalObjective")).toHaveValue("Original objective");
  await page.locator("#goalStatus").selectOption("paused");
  goal.objective = "Updated by native client";
  await page.locator("#goalsQueueRefresh").click();
  await expect(page.locator("#goalSummary")).toContainText("Updated by native client");
  await page.locator("#goalSave").click();
  await expect(page.locator("#goalObjective")).toHaveValue("Updated by native client");
  expect(commands).toEqual([{ action:"set_goal", status:"paused" }]);
});
