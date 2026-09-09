import { test, expect, type Page } from "@playwright/test";
import path from "node:path";
import { SCRIPTED_REPLY, login } from "../tests/helpers";

const OUT_DIR = process.env.SCREENSHOT_DIR ?? path.resolve(process.cwd(), "..", "..", "docs", "screenshots");

async function openThread(page: Page) {
  await login(page);
  if (await page.locator("#btnMenu").isVisible()) await page.locator("#btnMenu").click();
  await page.locator(".proj", { hasText:"Demo" }).locator(".project-add").click();
  await page.locator("#input").fill("Review this project's next release.");
  await page.locator("#sendBtn").click();
  await expect(page.locator(".msg.agent", { hasText:SCRIPTED_REPLY })).toBeVisible();
  await expect(page.locator("#stopBtn")).toBeHidden();
}

test("native goals and queue", async ({ page }, testInfo) => {
  await openThread(page);
  await page.locator("#goalsQueueBtn").click();
  await expect(page.locator("#goalSave")).toBeEnabled();
  await page.locator("#goalObjective").fill("Prepare the release with passing checks and updated documentation.");
  await page.locator("#goalBudget").fill("50000");
  await page.locator("#goalSave").click();
  await expect(page.locator("#goalsQueueNotice")).toHaveText("Saved.");
  await page.locator("#queueText").fill("Check the migration guide against the new API.");
  await page.locator("#queueSave").click();
  await expect(page.locator("#goalQueueList li")).toHaveCount(1);
  await page.locator("#queueText").fill("Review the release notes for missing changes.");
  await page.locator("#queueSave").click();
  await expect(page.locator("#goalQueueList li")).toHaveCount(2);
  await page.locator(".goals-queue-dialog .content").evaluate(el => { el.scrollTop = 0; });
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  const goalFile = path.join(OUT_DIR, `native-goal-${testInfo.project.name}.png`);
  await page.screenshot({ path:goalFile, animations:"disabled" });
  await testInfo.attach("native goal", { path:goalFile, contentType:"image/png" });
  await page.locator("#queueStart").scrollIntoViewIfNeeded();
  const queueFile = path.join(OUT_DIR, `native-queue-${testInfo.project.name}.png`);
  await page.screenshot({ path:queueFile, animations:"disabled" });
  await testInfo.attach("native queue", { path:queueFile, contentType:"image/png" });
});

test("rich MCP form", async ({ page }, testInfo) => {
  await openThread(page);
  // Use the same typed native request fixture as the rich-form browser tests; no form is submitted.
  await page.evaluate(() => {
    const selected = JSON.parse(localStorage.getItem("giskard.lastThread")!);
    (window as any).handleEvent({ kind:"server_request_received", thread:selected.tid, turn:null, request:{
      id:"screenshot-release-form", method:"mcpServer/elicitation/request", received_at:"2026-09-09T12:00:00Z",
      params:{ threadId:"native", turnId:null, serverName:"release-tools", mode:"openai/form",
        message:"Configure the release review", requestedSchema:{ type:"object", required:["review"], properties:{
          review:{ type:"object", title:"Review settings", required:["name", "checks", "enabled"], properties:{
            name:{ type:"string", title:"Release name", default:"September release" },
            checks:{ type:"integer", title:"Required checks", minimum:1, default:3 },
            enabled:{ type:"boolean", title:"Include documentation", default:true }
          } }, notes:{ type:"string", title:"Additional notes" }
        } }
      }
    } });
  });
  const card = page.locator(".msg.server-request");
  await expect(card.getByLabel("Release name", { exact:true })).toHaveValue("September release");
  await card.scrollIntoViewIfNeeded();
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  const file = path.join(OUT_DIR, `native-mcp-form-${testInfo.project.name}.png`);
  await page.screenshot({ path:file, animations:"disabled" });
  await testInfo.attach("rich MCP form", { path:file, contentType:"image/png" });
});

test("model capabilities and service tier", async ({ page }, testInfo) => {
  await openThread(page);
  await page.locator("#modelPickerBtn").click();
  await page.locator("#serviceTierSel").selectOption("priority");
  await expect(page.locator("#modelPickerBtn")).toContainText("Priority");
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  const file = path.join(OUT_DIR, `native-model-${testInfo.project.name}.png`);
  await page.screenshot({ path:file, animations:"disabled" });
  await testInfo.attach("model service tier", { path:file, contentType:"image/png" });
});
