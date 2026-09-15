import { test, expect, type Page } from "@playwright/test";
import { login, SCRIPTED_REPLY } from "./helpers";

async function openThread(page: Page) {
  await login(page);
  await page.locator(".proj", { hasText: "Demo" }).locator(".project-add").click();
  await page.locator("#input").fill("Prepare async question UI tests.");
  await page.locator("#sendBtn").click();
  await expect(page.locator(".msg.agent", { hasText: SCRIPTED_REPLY })).toBeVisible();
  await expect(page.locator("#stopBtn")).toBeHidden();
}

// Browser fault injection exercises delivery acknowledgement, replay, and lost connection states
// without relying on timing the native harness. The separate integration scenario uses real WS.
async function injectQuestions(page: Page, active = false, options: string[] = ["First", "Second"]) {
  await page.evaluate(({ active, options }) => {
    const app = window as any;
    const selected = JSON.parse(localStorage.getItem("giskard.lastThread")!);
    app.__questionMessages = [];
    app.send = (message: unknown) => { app.__questionMessages.push(message); return true; };
    app.handleServer({ type:"thread_capabilities", thread_id:selected.tid, turn_steering:true }, {});
    if (active) app.handleEvent({ kind:"turn_started", turn:"question-turn", thread:selected.tid });
    app.__questionItem = { id:"question-item", harness_item_id:"native-question", payload:{
      kind:"agent_message", text:"", questions:[{title:"Which branch?", options}]
    }};
    app.addItem(app.__questionItem, "question-turn");
  }, { active, options });
}

async function messages(page: Page) {
  return page.evaluate(() => (window as any).__questionMessages);
}

test("questions without prose default to the first option but require explicit submit", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page);
  const card = page.locator(".agent-questions");
  await expect(card.getByRole("radio", { name:"First" })).toBeChecked();
  expect(await messages(page)).toEqual([]);
  await card.getByRole("button", { name:"Send answer" }).click();
  expect(await messages(page)).toMatchObject([{ type:"send_input", text:"Which branch?\nFirst" }]);
  await expect(card.locator("button[type=submit]")).toBeDisabled();
  await page.evaluate(() => (window as any).addItem((window as any).__questionItem, "question-turn"));
  await expect(page.locator(".agent-questions")).toHaveCount(1);
  await expect(card.locator("button[type=submit]")).toBeDisabled();
  expect(await messages(page)).toHaveLength(1);
});

test("free text overrides the selected option and acceptance survives replay and reload", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  const card = page.locator(".agent-questions");
  await card.getByRole("textbox").fill("Use a new branch");
  await card.getByRole("button", { name:"Send answer" }).click();
  expect(await messages(page)).toMatchObject([{ type:"steer_input", expected_turn_id:"question-turn", text:"Which branch?\nUse a new branch" }]);
  await page.evaluate(() => {
    const app = window as any;
    const sent = app.__questionMessages[0];
    app.handleServer({ type:"steer_input_accepted", thread_id:sent.thread_id, request_id:sent.request_id, turn_id:sent.expected_turn_id }, {});
    app.addItem(app.__questionItem, "question-turn");
  });
  await expect(card).toContainText("Answer sent from this browser.");
  await page.reload();
  await expect(page.locator("#input")).toBeVisible();
  await injectQuestions(page);
  await expect(card).toContainText("Answer sent from this browser.");
  await expect(card.locator("button[type=submit]")).toBeDisabled();
  expect(await messages(page)).toEqual([]);
});

test("free-text-only questions preserve drafts after rejection and uncertain delivery", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true, []);
  const card = page.locator(".agent-questions");
  await card.getByRole("textbox").fill("My branch");
  await card.locator("button[type=submit]").click();
  await page.evaluate(() => {
    const app = window as any;
    const sent = app.__questionMessages[0];
    app.handleServer({ type:"error", action:"steer_input", thread_id:sent.thread_id, request_id:sent.request_id, message:"Turn already ended" }, {});
  });
  await expect(card.getByRole("textbox")).toHaveValue("My branch");
  await expect(card.locator("button[type=submit]")).toBeEnabled();
  await card.locator("button[type=submit]").click();
  await page.evaluate(() => (window as any).markQuestionDeliveryUncertain());
  await expect(card).toContainText("Delivery is uncertain");
  await expect(card.locator("button[type=submit]")).toBeDisabled();
  await page.evaluate(() => (window as any).addItem((window as any).__questionItem, "question-turn"));
  expect(await messages(page)).toHaveLength(2);
  await expect(card.getByRole("textbox")).toHaveValue("My branch");
});

test("composer steering retains changed drafts on acceptance and rejected drafts on failure", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  const input = page.locator("#input");
  await input.fill("Please also check tests");
  await page.locator("#sendBtn").click();
  await expect(input).toHaveValue("Please also check tests");
  await input.fill("Another thought");
  await page.evaluate(() => {
    const app = window as any;
    const sent = app.__questionMessages[0];
    app.handleServer({ type:"steer_input_accepted", thread_id:sent.thread_id, request_id:sent.request_id, turn_id:sent.expected_turn_id }, {});
  });
  await expect(input).toHaveValue("Another thought");
  await page.locator("#sendBtn").click();
  await page.evaluate(() => {
    const app = window as any;
    const sent = app.__questionMessages[1];
    app.handleServer({ type:"error", action:"steer_input", thread_id:sent.thread_id, request_id:sent.request_id, message:"Rejected" }, {});
  });
  await expect(input).toHaveValue("Another thought");
  await expect(page.locator("#sendBtn")).toBeEnabled();
});

for (const active of [true, false]) {
  test(`native question answer reaches the transcript while ${active ? "running" : "idle"}`, async ({ page }) => {
    await openThread(page);
    await page.locator("#input").fill(active ? "Ask a scripted async question." : "Ask a scripted question and finish.");
    await page.locator("#sendBtn").click();
    const card = page.locator(".agent-questions");
    await expect(card).toContainText("Which branch should I use for the implementation?");
    if (!active) await expect(page.locator("#stopBtn")).toBeHidden();
    await card.getByRole("textbox").fill("Create branch async-capabilities");
    await card.getByRole("button", { name:"Send answer" }).click();
    await expect(page.locator(".msg.user", { hasText:"Create branch async-capabilities" })).toHaveCount(1);
    await expect(card).toContainText("Answer sent from this browser.");
    await page.reload();
    await expect(page.locator(".msg.user", { hasText:"Create branch async-capabilities" })).toHaveCount(1);
    await expect(card.locator("button[type=submit]")).toBeDisabled();
  });
}

test("an active child question can be answered while its composer and historical questions remain read-only", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  await page.evaluate(() => {
    const app = window as any;
    app.managedThreadReadOnly = () => true;
    app.updateComposerControls();
    app.addItem({ ...app.__questionItem, id:"old-question", harness_item_id:"old-native-question" }, "old-turn");
  });
  await expect(page.locator("#input")).toBeDisabled();
  const cards = page.locator(".agent-questions");
  await expect(cards.nth(0).locator("button[type=submit]")).toBeEnabled();
  await expect(cards.nth(1).locator("button[type=submit]")).toBeDisabled();
  await expect(cards.nth(1)).toContainText("This thread is read-only");
  await cards.nth(0).locator("button[type=submit]").click();
  expect(await messages(page)).toMatchObject([{ type:"steer_input", question_item_id:"question-item" }]);
});

test("steering refuses attachments and preserves the composer", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  await page.locator("#input").fill("My draft with a file");
  await page.locator('input[type="file"]').setInputFiles({ name:"note.txt", mimeType:"text/plain", buffer:Buffer.from("note") });
  await expect(page.locator("#sendBtn")).toBeEnabled();
  await page.locator("#sendBtn").click();
  expect(await messages(page)).toEqual([]);
  await expect(page.locator("#input")).toHaveValue("My draft with a file");
  await expect(page.locator("#notices")).toContainText("Attachments cannot be sent to a running turn");
});

test("a timeout cannot make an echoed answer retryable", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  const card = page.locator(".agent-questions");
  await card.locator("button[type=submit]").click();
  await page.evaluate(() => {
    const app = window as any;
    const sent = app.__questionMessages[0];
    app.confirmQuestionEcho(sent.text, sent.expected_turn_id, "giskard-steer:" + sent.request_id);
    app.handleServer({ type:"error", action:"steer_input", code:"harness_timeout", thread_id:sent.thread_id, request_id:sent.request_id, message:"Delivery unknown" }, {});
  });
  await expect(card).toContainText("Answer sent from this browser.");
  await expect(card.locator("button[type=submit]")).toBeDisabled();
});

test("a timeout without an echo leaves question delivery uncertain", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  const card = page.locator(".agent-questions");
  await card.locator("button[type=submit]").click();
  await page.evaluate(() => {
    const app = window as any;
    const sent = app.__questionMessages[0];
    app.handleServer({ type:"error", action:"steer_input", code:"harness_timeout", thread_id:sent.thread_id, request_id:sent.request_id, message:"Delivery unknown" }, {});
  });
  await expect(card).toContainText("Delivery is uncertain");
  await expect(card.locator("button[type=submit]")).toBeDisabled();
});

test("switching threads while steering is pending releases the new composer and preserves the old draft", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  await page.locator("#input").fill("Pending steering draft");
  await page.locator("#sendBtn").click();
  await page.locator(".proj", { hasText:"Demo" }).locator(".project-add").click();
  await page.locator("#input").fill("New thread draft");
  await expect(page.locator("#sendBtn")).toBeEnabled();
});

test("replaying an identical answer from an earlier turn does not confirm a new answer", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  const card = page.locator(".agent-questions");
  await card.locator("button[type=submit]").click();
  await page.evaluate(() => {
    const app = window as any;
    app.markQuestionDeliveryUncertain();
    app.confirmQuestionEcho(app.__questionMessages[0].text, "old-turn");
  });
  await expect(card).toContainText("Delivery is uncertain");
});

test("an attachment prompt keeps its display when native input and steering replay in the same turn", async ({ page }) => {
  await openThread(page);
  await page.evaluate(() => {
    const app = window as any;
    app.handleEvent({ kind:"turn_started", turn:"attachment-turn" });
    app.renderLiveTurnUserInput("attachment-turn", { text:"Read the note", attachments:[{ name:"note.txt", mime_type:"text/plain", size:4, kind:"file" }] });
    app.addItem({ id:"native-initial", harness_item_id:"native-initial", payload:{ kind:"user_message", text:"Read the note\n\nAttached files available on the harness host:\n- /private/upload/note.txt" } }, "attachment-turn");
    app.addItem({ id:"steered", harness_item_id:"steered", payload:{ kind:"user_message", text:"Also run tests" } }, "attachment-turn");
  });
  const turn = page.locator('.msg.user[data-turn="attachment-turn"]');
  await expect(turn).toHaveCount(2);
  await expect(turn.first()).toContainText("note.txt");
  await expect(turn.first()).not.toContainText("/private/upload");
  await expect(turn.nth(1)).toContainText("Also run tests");
});


test("uncertain delivery requires checking the transcript and a separate explicit resubmit", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  const card = page.locator(".agent-questions");
  await card.getByRole("textbox").fill("My own answer");
  await card.locator("button[type=submit]").click();
  await page.evaluate(() => (window as any).markQuestionDeliveryUncertain());
  await card.getByRole("button", { name:"I checked the transcript — edit and retry" }).click();
  expect(await messages(page)).toHaveLength(1);
  await expect(card.getByRole("textbox")).toHaveValue("My own answer");
  await card.locator("button[type=submit]").click();
  expect(await messages(page)).toHaveLength(2);
});


test("an identical old answer in the same turn cannot confirm a new steering request", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  const card = page.locator(".agent-questions");
  await card.locator("button[type=submit]").click();
  await page.evaluate(() => {
    const app = window as any;
    const sent = app.__questionMessages[0];
    app.markQuestionDeliveryUncertain();
    app.confirmQuestionEcho(sent.text, sent.expected_turn_id, "earlier-request");
  });
  await expect(card).toContainText("Delivery is uncertain");
});

test("an exact native composer echo confirms delivery even if the acknowledgement is lost", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  await page.locator("#input").fill("Confirm through native echo");
  await page.locator("#sendBtn").click();
  await page.evaluate(() => {
    const app = window as any;
    const sent = app.__questionMessages[0];
    app.confirmQuestionEcho(sent.text, sent.expected_turn_id, "giskard-steer:" + sent.request_id);
    app.handleServer({ type:"error", action:"steer_input", code:"harness_timeout", thread_id:sent.thread_id, request_id:sent.request_id, message:"Acknowledgement lost" }, {});
  });
  await expect(page.locator("#input")).toHaveValue("");
});

test("initial input IDs from other native clients still reconcile the original prompt", async ({ page }) => {
  await openThread(page);
  await page.evaluate(() => {
    const app = window as any;
    app.handleEvent({ kind:"turn_started", turn:"external-client-turn" });
    app.renderLiveTurnUserInput("external-client-turn", { text:"An imported prompt", attachments:[] });
    app.addItem({ id:"external-initial", harness_item_id:"external-initial", payload:{ kind:"user_message", text:"An imported prompt", client_id:"another-client-start-id" } }, "external-client-turn");
    app.addItem({ id:"giskard-steer", harness_item_id:"giskard-steer", payload:{ kind:"user_message", text:"An imported prompt", client_id:"giskard-steer:my-request" } }, "external-client-turn");
  });
  await expect(page.locator('.msg.user[data-turn="external-client-turn"]')).toHaveCount(2);
});

test("multiple questions are answered together and an unanswered free-text question prevents submission", async ({ page }) => {
  await openThread(page);
  await injectQuestions(page, true);
  await page.evaluate(() => {
    const app = window as any;
    app.__questionItem.payload.questions.push({ title:"Any constraints?" });
    app.addItem(app.__questionItem, "question-turn");
  });
  const card = page.locator(".agent-questions");
  await card.locator("button[type=submit]").click();
  expect(await messages(page)).toEqual([]);
  await card.getByRole("textbox").nth(1).fill("Keep existing APIs");
  await card.locator("button[type=submit]").click();
  expect(await messages(page)).toMatchObject([{ text:"Which branch?\nFirst\n\nAny constraints?\nKeep existing APIs" }]);
});
