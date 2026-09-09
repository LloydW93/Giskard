import { test, expect, type Page, type Route } from "@playwright/test";
import { login, SCRIPTED_REPLY, selectedThread } from "./helpers";

const endpoint = /\/api\/projects\/[^/]+\/threads\/[^/]+\/context-window$/;
const fixture = {
  model: { provider:"replay", model:"replay-model", reasoning_effort:"high" },
  advertised_maximum:1000000, default_window:272000, selected_window:272000,
  override_window:null as number | null, non_premium_window:272000, effective_window:258400,
  can_configure:true,
};
async function createThread(page: Page, prompt = "Exercise session context limits.") {
  await page.locator(".proj", { hasText:"Demo" }).locator(".project-add").click();
  await page.locator("#input").fill(prompt);
  await page.locator("#sendBtn").click();
  await expect(page.locator(".msg.agent", { hasText:SCRIPTED_REPLY })).toBeVisible();
  await expect(page.locator("#stopBtn")).toBeHidden();
}
async function openMenu(page: Page) {
  await page.locator("#usageBtn").click();
  await expect(page.locator("#contextWindowMode")).toBeVisible();
}

test("context edits survive usage updates without another catalog request", async ({ page }) => {
  let reads = 0;
  let body: any;
  await page.route(endpoint, async route => {
    if (route.request().method() === "POST") {
      body = route.request().postDataJSON();
      await route.fulfill({ json:{ ...fixture, selected_window:body.context_window, override_window:body.context_window } });
    } else { reads++; await route.fulfill({ json:fixture }); }
  });
  await login(page);
  await createThread(page);
  await openMenu(page);
  await expect(page.locator("#contextWindowMode")).toHaveValue("default");
  await expect(page.locator("#contextWindowValue")).toBeDisabled();
  await page.locator("#contextWindowMode").selectOption("custom");
  const input = page.locator("#contextWindowValue");
  await input.fill("500000");
  await page.evaluate(() => {
    (window as any).updateGauge(12345, 258400);
    (window as any).renderTokens({ total:{ input:15000, output:500, total:15500 } });
  });
  await expect(input).toBeFocused();
  await expect(input).toHaveValue("500000");
  await expect(page.locator("#usageCurrentValues")).toHaveText("12.3k / 258.4k");
  await expect(page.locator("#contextWindowPremium")).toContainText("premium long-context rates");
  await expect(page.locator("#contextWindowPremium")).toContainText("not a price guarantee");
  expect(reads).toBe(1);
  await page.locator("#contextWindowSave").click();
  await expect(page.locator("#contextWindowStatus")).toContainText("Saved for the next turn");
  expect(body).toEqual({ model:fixture.model, context_window:500000 });
  await expect(page.locator("#gauge")).toHaveText("12.3k / 258.4k");
  await page.locator("#contextWindowMode").selectOption("default");
  await page.locator("#contextWindowSave").click();
  await expect.poll(() => body.context_window).toBeNull();
});

test("rejects out-of-range input and preserves edits after a failed save", async ({ page }) => {
  let posts = 0;
  await page.route(endpoint, async route => {
    if (route.request().method() === "POST") {
      posts++; await route.fulfill({ status:409, json:{ error:"model_changed", message:"The model changed. Reopen the context menu." } });
    } else await route.fulfill({ json:fixture });
  });
  await login(page);
  await createThread(page);
  await openMenu(page);
  await page.locator("#contextWindowMode").selectOption("custom");
  for (const value of ["271999", "1000001", "272000.5", ""]) {
    await page.locator("#contextWindowValue").fill(value);
    await page.locator("#contextWindowSave").click();
    await expect(page.locator("#contextWindowStatus")).toContainText("Enter a whole number");
  }
  expect(posts).toBe(0);
  await page.locator("#contextWindowValue").fill("1000000");
  await page.locator("#contextWindowSave").click();
  await expect(page.locator("#contextWindowStatus")).toContainText("Could not save context limit");
  await expect(page.locator("#contextWindowValue")).toHaveValue("1000000");
  await expect(page.locator("#contextWindowSave")).toBeEnabled();
  expect(posts).toBe(1);
});

test("unknown maximum and read-only sessions cannot edit limits", async ({ page }) => {
  let config = { ...fixture, advertised_maximum:null as number | null, can_configure:false };
  await page.route(endpoint, route => route.fulfill({ json:config }));
  await login(page);
  await createThread(page);
  await openMenu(page);
  await expect(page.locator("#contextWindowSave")).toBeDisabled();
  await expect(page.locator("#contextWindowSettings")).toContainText("has not advertised a maximum");
  await page.locator("#usageClose").click();
  config = { ...fixture, can_configure:false };
  // Exercise the browser's read-only guard even if a stale response says it is configurable.
  await page.evaluate(() => { (window as any).managedThreadReadOnly = () => true; });
  await openMenu(page);
  await expect(page.locator("#contextWindowMode")).toBeDisabled();
  await expect(page.locator("#contextWindowSave")).toBeDisabled();
  await expect(page.locator("#contextWindowSettings")).toContainText("This thread is read-only");
});

test("load errors are visible and retryable", async ({ page }) => {
  let reads = 0;
  await page.route(endpoint, route => ++reads === 1 ?
    route.fulfill({ status:503, json:{ error:"unavailable", message:"Catalog unavailable" } }) :
    route.fulfill({ json:fixture }));
  await login(page);
  await createThread(page);
  await page.locator("#usageBtn").click();
  await expect(page.locator("#contextWindowSettings")).toContainText("Could not load session context limit");
  await page.locator("#contextWindowRetry").click();
  await expect(page.locator("#contextWindowMode")).toHaveValue("default");
  expect(reads).toBe(2);
});

test("late load and save responses cannot overwrite another session", async ({ page }) => {
  let held: Route | null = null;
  let hold = true;
  await page.route(endpoint, async route => {
    if (hold) { held = route; return; }
    await route.fulfill({ json:fixture });
  });
  await login(page);
  await createThread(page, "First context session");
  const first = await selectedThread(page);
  await page.locator("#usageBtn").click();
  await expect.poll(() => held !== null).toBe(true);
  const load = held!;
  hold = false;
  await createThread(page, "Second context session");
  await openMenu(page);
  await load.fulfill({ json:{ ...fixture, selected_window:900000, override_window:900000 } });
  await expect(page.locator("#contextWindowMode")).toHaveValue("default");
  await page.locator("#usageClose").click();
  await page.locator(`.thread[data-tid="${first!.tid}"]`).click();
  await openMenu(page);
  await page.locator("#contextWindowMode").selectOption("custom");
  await page.locator("#contextWindowValue").fill("900000");
  hold = true;
  held = null;
  await page.locator("#contextWindowSave").click();
  await expect.poll(() => held !== null).toBe(true);
  const save = held!;
  hold = false;
  await createThread(page, "Third context session");
  await openMenu(page);
  await save.fulfill({ json:{ ...fixture, selected_window:900000, override_window:900000 } });
  await expect(page.locator("#contextWindowMode")).toHaveValue("default");
  await expect(page.locator("#contextWindowStatus")).toBeEmpty();
});

test("metadata changes refresh limits but usage-only metadata preserves edits", async ({ page }) => {
  let reads = 0;
  let config = { ...fixture };
  await page.route(endpoint, route => { reads++; return route.fulfill({ json:config }); });
  await login(page);
  await createThread(page);
  await openMenu(page);
  await page.locator("#contextWindowMode").selectOption("custom");
  await page.locator("#contextWindowValue").fill("500000");
  const selected = await selectedThread(page);
  await page.evaluate(tid => {
    const app = window as any;
    const current = app.composedThreadDetail(tid);
    app.applyThreadMetadata({ ...current, revision:current.revision + 1, context_window:260000 });
  }, selected!.tid);
  await expect(page.locator("#contextWindowValue")).toHaveValue("500000");
  expect(reads).toBe(1);
  config = { ...fixture, selected_window:700000, override_window:700000 };
  await page.evaluate(tid => {
    const app = window as any;
    const current = app.composedThreadDetail(tid);
    app.applyThreadMetadata({ ...current, revision:current.revision + 1, context_window_override:700000 });
  }, selected!.tid);
  await expect(page.locator("#contextWindowValue")).toHaveValue("700000");
  expect(reads).toBe(2);
});

test("session context selection persists through the real API and reload", async ({ page }) => {
  await login(page);
  await createThread(page);
  await openMenu(page);
  await expect(page.locator("#contextWindowMode")).toBeEnabled();
  const input = page.locator("#contextWindowValue");
  const maximum = await input.getAttribute("max");
  await page.locator("#contextWindowMode").selectOption("custom");
  await input.fill(maximum!);
  await page.locator("#contextWindowSave").click();
  await expect(page.locator("#contextWindowStatus")).toContainText("Saved for the next turn");
  await page.reload();
  await openMenu(page);
  await expect(page.locator("#contextWindowMode")).toHaveValue("custom");
  await expect(input).toHaveValue(maximum!);
  await page.locator("#contextWindowMode").selectOption("default");
  await page.locator("#contextWindowSave").click();
  await expect(page.locator("#contextWindowStatus")).toContainText("Saved for the next turn");
  await page.reload();
  await openMenu(page);
  await expect(page.locator("#contextWindowMode")).toHaveValue("default");
});
