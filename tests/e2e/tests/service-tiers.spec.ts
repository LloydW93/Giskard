import { test, expect } from "@playwright/test";
import { login, SCRIPTED_REPLY } from "./helpers";

test.describe("model service tiers", () => {
  test.beforeEach(async ({ page }) => { await login(page); });

  test("retains explicit tiers across effort changes and catalog refresh, then sends them", async ({ page }) => {
    await page.locator(".proj", { hasText: "Demo" }).locator(".project-add").click();
    await expect(page.locator("#modelPickerBtn")).toContainText("Replay Model");
    await page.locator("#modelPickerBtn").click();
    const tier = page.getByRole("combobox", { name: "Service tier", exact: true });
    await expect(tier).toHaveValue("");
    await expect(tier.locator("option")).toHaveText(["Native default", "Standard", "Priority"]);
    await expect(page.locator("#modelCapabilities")).toContainText("Inputs: text, image, audio");
    await expect(page.locator("#modelCapabilities")).toContainText("Multi-agent: v2");
    await tier.selectOption("priority");
    await page.locator("#effortSel").selectOption("high");
    await expect(tier).toHaveValue("priority");
    const refreshed = page.waitForResponse(r => r.url().endsWith("/models") && r.request().method() === "GET");
    await page.locator("#refreshModels").click();
    expect((await refreshed).ok()).toBeTruthy();
    await expect(tier).toHaveValue("priority");
    await expect(page.locator("#modelPickerBtn")).toContainText("Priority");
    await page.keyboard.press("Escape");
    await page.locator("#input").fill("Service tier persists with this turn");
    const started = page.waitForRequest(r => r.method() === "POST" && r.url().endsWith("/threads/start"));
    await page.locator("#sendBtn").click();
    const request = await started;
    expect(request.postDataJSON().model_ref).toMatchObject({service_tier:"priority", reasoning_effort:"high"});
    // The composer remains enabled while draft creation is in flight. Wait for the real
    // thread and completed turn before reloading its persisted model selection.
    await expect(page.locator(".msg.agent", { hasText:SCRIPTED_REPLY })).toBeVisible();
    await expect(page.locator("#stopBtn")).toBeHidden();
    await page.reload();
    await expect(page.locator("#modelPickerBtn")).toContainText("Priority");
    await page.locator("#modelPickerBtn").click();
    await tier.selectOption("");
    await expect(page.locator("#modelPickerBtn")).not.toContainText("Priority");
    await expect(tier).toBeEnabled();
    await page.reload();
    await page.locator("#modelPickerBtn").click();
    await expect(tier).toHaveValue("");
  });
});

test("changing models clears the prior tier and hides unsupported controls", async ({ page }) => {
  await page.route(/\/api\/projects\/[^/?]+\/models$/, async route => {
    const response = await route.fetch();
    const body = await response.json();
    body.models.push({ provider:"replay", model:"without-tiers", display_name:"Without tiers",
      context_window:128000, supports_reasoning_effort:false });
    await route.fulfill({ response, json:body });
  });
  await login(page);
  await page.locator(".proj", { hasText:"Demo" }).locator(".project-add").click();
  await expect(page.locator("#modelPickerBtn")).toContainText("Replay Model");
  await page.locator("#modelPickerBtn").click();
  await page.locator("#serviceTierSel").selectOption("priority");
  await page.locator("#modelSel").selectOption("replay/without-tiers");
  await expect(page.locator("#serviceTierControl")).toBeHidden();
  await expect(page.locator("#modelPickerBtn")).not.toContainText("Priority");
  await page.locator("#modelSel").selectOption("replay/gpt-6-astra");
  await expect(page.locator("#serviceTierSel")).toHaveValue("");
});
