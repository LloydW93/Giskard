import { test, expect } from "@playwright/test";
import { login, SCRIPTED_REPLY } from "./helpers";

const wav = Buffer.from("524946462600000057415645666d74201000000001000100401f0000803e00000200100064617461020000000000", "hex");

test("audio attachment uses native kind and persists its descriptor on reload", async ({ page }) => {
  await login(page);
  await page.locator(".proj", { hasText: "Demo" }).locator(".project-add").click();
  await page.locator("#attachmentInput").setInputFiles({ name: "voice.wav", mimeType: "audio/x-wav", buffer: wav });
  await expect(page.locator(".attachment-chip")).toContainText("Audio: voice.wav");
  const request = page.waitForRequest(r => r.url().endsWith("/threads/start") && r.method() === "POST");
  await page.locator("#sendBtn").click();
  const body = (await request).postDataJSON();
  expect(body.attachments[0]).toMatchObject({ kind: "audio", mime_type: "audio/wav", data_base64: wav.toString("base64") });
  await expect(page.locator("#transcript .msg.agent", { hasText: SCRIPTED_REPLY })).toBeVisible();
  await page.reload();
  await expect(page.locator("#transcript .msg.user", { hasText: "voice.wav" })).toBeVisible();
});

test("unsupported audio container remains a file with an explicit notice", async ({ page }) => {
  await login(page);
  await page.locator(".proj", { hasText: "Demo" }).locator(".project-add").click();
  await page.locator("#attachmentInput").setInputFiles({ name: "voice.ogg", mimeType: "audio/ogg", buffer: Buffer.from("OggSrecording") });
  await expect(page.locator(".attachment-chip")).toContainText("voice.ogg");
  await expect(page.locator(".attachment-chip")).not.toContainText("Audio:");
  await expect(page.getByText(/this audio format is attached as a file/)).toBeVisible();
});

test("known non-audio model blocks sending a recording", async ({ page }) => {
  await page.route(/\/api\/projects\/[^/?]+\/models$/, async route => {
    const response = await route.fetch();
    const body = await response.json();
    for (const model of body.models) model.input_modalities = ["text"];
    await route.fulfill({ response, json: body });
  });
  await login(page);
  await page.locator(".proj", { hasText: "Demo" }).locator(".project-add").click();
  await page.locator("#attachmentInput").setInputFiles({ name: "voice.wav", mimeType: "audio/wav", buffer: wav });
  await expect(page.locator(".attachment-chip")).toContainText("Audio: voice.wav");
  await expect(page.locator("#sendBtn")).toBeDisabled();
  await expect(page.locator("#sendBtn")).toHaveAttribute("title", /does not accept audio/);
  await page.locator("#input").press("Enter");
  await expect(page.locator("#notices")).toContainText("does not accept audio");
  await page.getByRole("button", { name: "Remove voice.wav" }).click();
  await page.locator("#input").fill("Text still works");
  await expect(page.locator("#sendBtn")).toBeEnabled();
});
