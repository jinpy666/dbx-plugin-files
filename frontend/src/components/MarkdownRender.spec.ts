// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import MarkdownRender from "./MarkdownRender.vue";

function render(text: string): VueWrapper {
  return mount(MarkdownRender, { props: { text } });
}

describe("MarkdownRender (轻量白名单渲染)", () => {
  // 安全模型：全文先转义再匹配语法，<script> 只能以文本形态出现。
  it("escapes raw HTML so <script> in md never produces a script element", () => {
    const wrapper = render("# hi\n\nhello <script>alert(1)</script> world");
    expect(wrapper.find("script").exists()).toBe(false);
    expect(wrapper.get(".wb-md-body").html()).not.toContain("<script");
    expect(wrapper.get(".wb-md-body").text()).toContain("<script>alert(1)</script>");
  });

  it("renders the whitelist subset: heading, bold, italic, inline code, lists, quote, hr, fence, link", () => {
    const wrapper = render(
      [
        "# Title",
        "",
        "Some **bold** and *italic* and `code`.",
        "",
        "- alpha",
        "- beta",
        "",
        "1. one",
        "2. two",
        "",
        "> quoted text",
        "",
        "---",
        "",
        "```js",
        "const x = 1 < 2;",
        "```",
        "",
        "[docs](https://example.com/a?b=1&c=2)",
      ].join("\n"),
    );
    const root = wrapper.get(".wb-md-body");
    expect(root.get("h1").text()).toBe("Title");
    expect(root.get("strong").text()).toBe("bold");
    expect(root.get("em").text()).toBe("italic");
    expect(root.get("code.wb-md-code-inline").text()).toBe("code");
    expect(root.findAll("ul li").map((li) => li.text())).toEqual(["alpha", "beta"]);
    expect(root.findAll("ol li").map((li) => li.text())).toEqual(["one", "two"]);
    expect(root.get("blockquote").text()).toContain("quoted text");
    expect(root.find("hr.wb-md-hr").exists()).toBe(true);
    // 围栏码内部不再做行内处理，且尖括号已转义。
    expect(root.get("pre.wb-md-code code").text()).toBe("const x = 1 < 2;");
    const link = root.get("a.wb-md-link");
    // 属性值为转义 &amp; 落盘、读取时实体解码，语义等价于 &。
    expect(link.attributes("href")).toBe("https://example.com/a?b=1&c=2");
    expect(link.text()).toBe("docs");
  });

  it("keeps javascript: links as plain text instead of anchors", () => {
    const wrapper = render("click [here](javascript:alert(1)) now");
    expect(wrapper.find("a").exists()).toBe(false);
    expect(wrapper.get(".wb-md-body").text()).toContain("javascript:alert(1)");
  });

  it("falls back to the raw escaped text for unrecognized syntax", () => {
    const wrapper = render("just a | table | row\nand ~~strike~~ text");
    const root = wrapper.get(".wb-md-body");
    expect(root.find("table").exists()).toBe(false);
    expect(root.text()).toContain("just a | table | row");
    expect(root.text()).toContain("~~strike~~");
  });

  it("never throws on pathological input and keeps the render container mounted", () => {
    const wrapper = render("```\n\n#".repeat(64) + "\n> \n- \n1. \n***\n<script>");
    expect(wrapper.find(".wb-md-render").exists()).toBe(true);
  });
});
