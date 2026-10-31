import test from "node:test";
import assert from "node:assert/strict";
import { executionTitle, executionTitleDetails } from "../src/lib/display-text.ts";

test("title previews discard a leading structured context block", () => {
  const title = "<in-app-browser-context source=\"ambient-ui-state\">Transient chrome</in-app-browser-context>\n\n## My request:\nReview the history view";
  assert.equal(executionTitle(title), "Review the history view");
});

test("attachment metadata is removed while following task text remains", () => {
  const title = "# Files mentioned by the user:\n\n## capture.png: /private/tmp/capture.png\nImage attachment: true\n\nReview the history view\n\nKeep the project and title visible";
  assert.equal(executionTitleDetails(title), "Review the history view\n\nKeep the project and title visible");
  assert.equal(executionTitle(title), "Review the history view");
});

test("ordinary titles and multi-line task descriptions remain readable", () => {
  const title = "Add a glossary entry\n\nKeep the terminology consistent";
  assert.equal(executionTitleDetails(title), title);
  assert.equal(executionTitle(title), "Add a glossary entry");
});

test("ordinary XML and document lists are retained", () => {
  assert.equal(executionTitleDetails("<button>Example</button>"), "<button>Example</button>");
  assert.equal(executionTitleDetails("# Files mentioned by the user:\nPlease summarize the uploads"), "# Files mentioned by the user:\nPlease summarize the uploads");
  assert.equal(executionTitle(""), "");
  assert.equal(executionTitle("0"), "0");
});

test("a leading Markdown link displays its label once", () => {
  const title = "[https://example.com/repository](https://example.com/repository) Review this project";
  assert.equal(executionTitle(title), "https://example.com/repository Review this project");
  assert.equal(executionTitleDetails(title), title);
  assert.equal(executionTitle("[review] project"), "[review] project");
});
