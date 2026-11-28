import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  compareNodes,
  ipv4Key,
  latencyClass,
  latencyRangeText,
  latencyText,
  nextSort,
  sortNodes,
} from "./format.ts";
import type { NodeRow } from "./api/types.ts";

function row(overrides: Partial<NodeRow> = {}): NodeRow {
  return {
    ip: "10.0.0.1",
    hostname: "node",
    pathKind: "direct",
    path: [],
    pathIncomplete: false,
    hops: 1,
    tunnels: ["udp"],
    natType: "PortRestricted",
    latencyMs: 20,
    version: "2.7.1-abcdef",
    proxyCidrs: [],
    ...overrides,
  };
}

function names(list: NodeRow[]): string[] {
  return list.map((node) => node.hostname);
}

describe("latencyText", () => {
  it("一律一位小数，不出现浮点尾巴", () => {
    assert.equal(latencyText(11), "11.0 ms");
    assert.equal(latencyText(11.04), "11.0 ms");
    assert.equal(latencyText(6.877000000000001), "6.9 ms");
    assert.equal(latencyText(27.607), "27.6 ms");
  });

  it("未知仍然是破折号", () => {
    assert.equal(latencyText(null), "—");
    assert.equal(latencyText(undefined), "—");
    assert.equal(latencyText(Number.NaN), "—");
    assert.equal(latencyText(Number.POSITIVE_INFINITY), "—");
  });
});

describe("latencyRangeText", () => {
  it("区间只写一次单位", () => {
    assert.equal(latencyRangeText(6.877000000000001, 27.607), "6.9 – 27.6 ms");
  });

  it("两端相同时只写一个数", () => {
    assert.equal(latencyRangeText(11, 11), "11.0 ms");
  });
});

describe("latencyClass", () => {
  it("按 <50 / 50–100 / ≥100 分档", () => {
    assert.equal(latencyClass(0), "fast");
    assert.equal(latencyClass(49.9), "fast");
    assert.equal(latencyClass(50), "medium");
    assert.equal(latencyClass(99.9), "medium");
    assert.equal(latencyClass(100), "slow");
    assert.equal(latencyClass(5000), "slow");
  });

  it("未知不进任何一档", () => {
    assert.equal(latencyClass(null), "unknown");
    assert.equal(latencyClass(undefined), "unknown");
    assert.equal(latencyClass(Number.NaN), "unknown");
  });
});

describe("nextSort", () => {
  it("三态循环：默认 → 正序 → 倒序 → 默认", () => {
    const first = nextSort(null, "name");
    assert.deepEqual(first, { key: "name", direction: "asc" });
    const second = nextSort(first, "name");
    assert.deepEqual(second, { key: "name", direction: "desc" });
    assert.equal(nextSort(second, "name"), null);
  });

  it("换一列从正序开始", () => {
    assert.deepEqual(nextSort({ key: "name", direction: "desc" }, "latency"), {
      key: "latency",
      direction: "asc",
    });
  });
});

describe("节点表排序", () => {
  it("节点名按自然序，数字段按数值比", () => {
    const list = [
      row({ ip: "10.0.0.10", hostname: "node10" }),
      row({ ip: "10.0.0.2", hostname: "node2" }),
      row({ ip: "10.0.0.3", hostname: "laptop" }),
    ];
    assert.deepEqual(names(sortNodes(list, "name", "asc")), ["laptop", "node2", "node10"]);
    assert.deepEqual(names(sortNodes(list, "name", "desc")), ["node10", "node2", "laptop"]);
  });

  it("地址按 IPv4 数值比", () => {
    assert.ok((ipv4Key("10.126.126.9") ?? 0) < (ipv4Key("10.126.126.10") ?? 0));
    assert.equal(ipv4Key("not-an-ip"), null);
    const list = [
      row({ ip: "10.126.126.10" }),
      row({ ip: "10.126.126.9" }),
      row({ ip: "10.126.126.2" }),
    ];
    assert.deepEqual(
      sortNodes(list, "ip", "asc").map((node) => node.ip),
      ["10.126.126.2", "10.126.126.9", "10.126.126.10"],
    );
  });

  it("路径按 直达 < 中继 < 未知 的稳定秩", () => {
    const list = [
      row({ ip: "10.0.0.1", hostname: "relay", pathKind: "relay" }),
      row({ ip: "10.0.0.2", hostname: "unknown", pathKind: "weird" }),
      row({ ip: "10.0.0.3", hostname: "direct", pathKind: "direct" }),
    ];
    assert.deepEqual(names(sortNodes(list, "route", "asc")), ["direct", "relay", "unknown"]);
    assert.deepEqual(names(sortNodes(list, "route", "desc")), ["unknown", "relay", "direct"]);
  });

  it("版本按数值段比，不按字符串", () => {
    const list = [
      row({ ip: "10.0.0.1", hostname: "a", version: "2.10.0-bbb" }),
      row({ ip: "10.0.0.2", hostname: "b", version: "2.7.1-aaa" }),
      row({ ip: "10.0.0.3", hostname: "c", version: "2.9.9-ccc" }),
    ];
    assert.deepEqual(names(sortNodes(list, "version", "asc")), ["b", "c", "a"]);
  });

  it("延迟未知的行正序倒序都排在最后", () => {
    const list = [
      row({ ip: "10.0.0.1", hostname: "unknown", latencyMs: null }),
      row({ ip: "10.0.0.2", hostname: "slow", latencyMs: 140 }),
      row({ ip: "10.0.0.3", hostname: "fast", latencyMs: 10 }),
    ];
    assert.deepEqual(names(sortNodes(list, "latency", "asc")), ["fast", "slow", "unknown"]);
    assert.deepEqual(names(sortNodes(list, "latency", "desc")), ["slow", "fast", "unknown"]);
  });

  it("同值行保持 core 给的相对顺序", () => {
    const list = [
      row({ ip: "10.0.0.1", hostname: "first", latencyMs: 10 }),
      row({ ip: "10.0.0.2", hostname: "second", latencyMs: 10 }),
      row({ ip: "10.0.0.3", hostname: "third", latencyMs: 10 }),
    ];
    assert.deepEqual(names(sortNodes(list, "latency", "asc")), ["first", "second", "third"]);
  });

  it("比较器可直接用，且方向可翻转", () => {
    const a = row({ hostname: "node2" });
    const b = row({ hostname: "node10" });
    assert.ok(compareNodes(a, b, "name", "asc") < 0);
    assert.ok(compareNodes(a, b, "name", "desc") > 0);
  });
});
