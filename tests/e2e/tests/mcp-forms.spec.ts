import { test, expect, type Page } from "@playwright/test";
import { login, SCRIPTED_REPLY } from "./helpers";

async function openForm(page: Page, schema: unknown, mode = "openai/form") {
  await login(page);
  await page.locator(".proj", { hasText: "Demo" }).locator(".project-add").click();
  await page.locator("#input").fill("Prepare form UI tests.");
  await page.locator("#sendBtn").click();
  await expect(page.locator(".msg.agent", { hasText: SCRIPTED_REPLY })).toBeVisible();
  await page.evaluate(({ schema, mode }) => {
    const app = window as any;
    const selected = JSON.parse(localStorage.getItem("giskard.lastThread")!);
    app.__formMessages = [];
    app.send = (message: unknown) => { app.__formMessages.push(message); return true; };
    app.__formEvent = { kind:"server_request_received", thread:selected.tid, turn:null, request:{
      id:"form-test", method:"mcpServer/elicitation/request", received_at:new Date().toISOString(),
      params:{ threadId:"native", turnId:null, serverName:"survey", mode, message:"Configure the job", requestedSchema:schema }
    }};
    app.handleEvent(app.__formEvent);
  }, { schema, mode });
  return page.locator(".msg.server-request");
}
async function sent(page: Page) { return page.evaluate(() => (window as any).__formMessages); }

test("nested forms retain defaults and submit exact typed content", async ({ page }) => {
  const card = await openForm(page, { type:"object", required:["job", "tags"], additionalProperties:false, properties:{
    job:{ type:"object", required:["name", "count", "enabled"], properties:{
      name:{ type:"string", title:"Job name", minLength:3, default:"daily" },
      count:{ type:"integer", title:"Count", minimum:1, maximum:9, default:2 },
      enabled:{ type:"boolean", title:"Enabled", default:false }
    }},
    tags:{ type:"array", title:"Tags", items:{type:"string", enum:["a", "b"]}, minItems:1, uniqueItems:true, default:["a"] },
    optional:{ type:"string", title:"Notes" }
  }});
  await expect(card.getByLabel("Job name", { exact:true })).toHaveValue("daily");
  await card.getByLabel("Count", { exact:true }).fill("3");
  await card.getByLabel("Tags (JSON)", { exact:true }).fill('["a", "b"]');
  await card.getByRole("button", { name:"Continue", exact:true }).click();
  expect(await sent(page)).toMatchObject([{ type:"server_request_response", request_id:"form-test", response:{
    kind:"result", value:{ action:"accept", content:{ job:{name:"daily", count:3, enabled:false}, tags:["a", "b"] } }
  }}]);
});

test("optional fields distinguish absent from false, empty and zero", async ({ page }) => {
  const card = await openForm(page, { type:"object", properties:{
    text:{ type:"string", title:"Text" }, number:{ type:"number", title:"Number" }, flag:{ type:"boolean", title:"Flag" }
  }});
  await card.getByLabel("Include Text", { exact:true }).check();
  await card.getByLabel("Include Number", { exact:true }).check();
  await card.getByLabel("Number", { exact:true }).fill("0");
  await card.getByRole("button", { name:"Continue", exact:true }).click();
  expect(await sent(page)).toMatchObject([{ response:{ value:{ content:{text:"", number:0} } } }]);
  expect((await sent(page))[0].response.value.content).not.toHaveProperty("flag");
});

test("composite schemas retain exact JSON values for server validation", async ({ page }) => {
  const card = await openForm(page, { oneOf:[
    { type:"object", required:["a"], properties:{ a:{type:"integer", minimum:1} }, additionalProperties:false },
    { type:"array", items:{type:"boolean"}, minItems:1 }
  ]});
  await card.locator("textarea").fill('[false,true]');
  await card.getByRole("button", { name:"Continue", exact:true }).click();
  expect(await sent(page)).toMatchObject([{ response:{ value:{ content:[false,true] } } }]);
});

for (const mode of ["form", "openai/form", "openaiForm"]) {
  test(`${mode} advanced schemas remain inspectable and can be declined`, async ({ page }) => {
    const card = await openForm(page, { type:"object", dependentSchemas:{ x:{ required:["y"] } } }, mode);
    await expect(card.locator("textarea")).toBeVisible();
    await expect(card).toContainText("dependentSchemas");
    await expect(card).toContainText("complete form schema is checked by the server");
    await card.getByRole("button", { name:"Decline", exact:true }).click();
    expect(await sent(page)).toMatchObject([{ response:{ value:{action:"decline", content:null} } }]);
  });
}

test("a failed send retains draft values and cancellation does not validate", async ({ page }) => {
  const card = await openForm(page, { type:"object", required:["text"], properties:{text:{type:"string", title:"Text", minLength:2}} });
  await card.getByLabel("Text", { exact:true }).fill("draft");
  await page.evaluate(() => { (window as any).send = () => false; });
  await card.getByRole("button", { name:"Continue", exact:true }).click();
  await expect(card.getByLabel("Text", { exact:true })).toHaveValue("draft");
  await expect(card.getByRole("button", { name:"Continue", exact:true })).toBeEnabled();
  await page.evaluate(() => {
    const app = window as any;
    app.send = (message: unknown) => { app.__formMessages.push(message); return true; };
    app.handleEvent(app.__formEvent);
  });
  await expect(page.locator(".msg.server-request")).toHaveCount(1);
  await card.getByLabel("Text", { exact:true }).fill("");
  await card.getByRole("button", { name:"Cancel", exact:true }).click();
  expect(await sent(page)).toMatchObject([{ response:{ value:{action:"cancel", content:null} } }]);
});

test("advanced local references preserve exact JSON and malformed JSON cannot submit", async ({ page }) => {
  const card = await openForm(page, { "$schema":"https://json-schema.org/draft/2020-12/schema", "$defs":{ count:{ type:"integer",minimum:1 } }, type:"object", properties:{count:{"$ref":"#/$defs/count"}} });
  await card.locator("textarea").fill("{broken");
  await card.getByRole("button", { name:"Continue", exact:true }).click();
  await expect(card.getByRole("alert")).toContainText("valid JSON");
  expect(await sent(page)).toEqual([]);
  await card.locator("textarea").fill('{"count":2}');
  await card.getByRole("button", { name:"Continue", exact:true }).click();
  expect(await sent(page)).toMatchObject([{response:{value:{content:{count:2}}}}]);
});

test("server validation failure reopens the form with its typed draft intact", async ({ page }) => {
  const card = await openForm(page, {type:"object",required:["count"],properties:{count:{type:"integer",title:"Count",minimum:1}}});
  await card.getByLabel("Count", {exact:true}).fill("0");
  await card.getByRole("button", {name:"Continue",exact:true}).click();
  await expect(card.getByRole("button", {name:"Continue",exact:true})).toBeDisabled();
  await page.evaluate(() => {
    const app = window as any;
    app.handleServer({type:"error",action:"server_request_response",message:"MCP form: /count: value must be at least 1"}, {});
  });
  await expect(card.getByLabel("Count", {exact:true})).toHaveValue("0");
  await expect(card.getByRole("button", {name:"Continue",exact:true})).toBeEnabled();
  await card.getByLabel("Count", {exact:true}).fill("2");
  await card.getByRole("button", {name:"Continue",exact:true}).click();
  expect(await sent(page)).toHaveLength(2);
  expect((await sent(page))[1].response.value.content).toEqual({count:2});
});
