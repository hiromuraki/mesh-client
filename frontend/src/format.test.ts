import { describe, expect, it } from "vitest";
import {
  compareNodes,
  ipv4Key,
  latencyClass,
  latencyRangeText,
  latencyText,
  nextSort,
  sortNodes,
} from "./format";
import type { NodeRow } from "./api/types";

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
    expect(latencyText(11)).toBe("11.0 ms");
    expect(latencyText(11.04)).toBe("11.0 ms");
    expect(latencyText(6.877000000000001)).toBe("6.9 ms");
    expect(latencyText(27.607)).toBe("27.6 ms");
  });

  it("未知仍然是破折号", () => {
    expect(latencyText(null)).toBe("—");
    expect(latencyText(undefined)).toBe("—");
    expect(latencyText(Number.NaN)).toBe("—");
    expect(latencyText(Number.POSITIVE_INFINITY)).toBe("—");
  });
});

describe("latencyRangeText", () => {
  it("区间只写一次单位", () => {
    expect(latencyRangeText(6.877000000000001, 27.607)).toBe("6.9 – 27.6 ms");
  });

  it("两端相同时只写一个数", () => {
    expect(latencyRangeText(11, 11)).toBe("11.0 ms");
  });
});

describe("latencyClass", () => {
  it("按 <50 / 50–100 / ≥100 分档", () => {
    expect(latencyClass(0)).toBe("fast");
    expect(latencyClass(49.9)).toBe("fast");
    expect(latencyClass(50)).toBe("medium");
    expect(latencyClass(99.9)).toBe("medium");
    expect(latencyClass(100)).toBe("slow");
    expect(latencyClass(5000)).toBe("slow");
  });

  it("未知不进任何一档", () => {
    expect(latencyClass(null)).toBe("unknown");
    expect(latencyClass(undefined)).toBe("unknown");
    expect(latencyClass(Number.NaN)).toBe("unknown");
  });
});

describe("nextSort", () => {
  it("三态循环：默认 → 正序 → 倒序 → 默认", () => {
    const first = nextSort(null, "name");
    expect(first).toEqual({ key: "name", direction: "asc" });
    const second = nextSort(first, "name");
    expect(second).toEqual({ key: "name", direction: "desc" });
    expect(nextSort(second, "name")).toBeNull();
  });

  it("换一列从正序开始", () => {
    expect(nextSort({ key: "name", direction: "desc" }, "latency")).toEqual({
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
    expect(names(sortNodes(list, "name", "asc"))).toEqual(["laptop", "node2", "node10"]);
    expect(names(sortNodes(list, "name", "desc"))).toEqual(["node10", "node2", "laptop"]);
  });

  it("地址按 IPv4 数值比", () => {
    expect(ipv4Key("10.126.126.9")).toBeLessThan(ipv4Key("10.126.126.10") ?? 0);
    expect(ipv4Key("not-an-ip")).toBeNull();
    const list = [
      row({ ip: "10.126.126.10" }),
      row({ ip: "10.126.126.9" }),
      row({ ip: "10.126.126.2" }),
    ];
    expect(sortNodes(list, "ip", "asc").map((node) => node.ip)).toEqual([
      "10.126.126.2",
      "10.126.126.9",
      "10.126.126.10",
    ]);
  });

  it("路径按 直达 < 中继 < 未知 的稳定秩", () => {
    const list = [
      row({ ip: "10.0.0.1", hostname: "relay", pathKind: "relay" }),
      row({ ip: "10.0.0.2", hostname: "unknown", pathKind: "weird" }),
      row({ ip: "10.0.0.3", hostname: "direct", pathKind: "direct" }),
    ];
    expect(names(sortNodes(list, "route", "asc"))).toEqual(["direct", "relay", "unknown"]);
    expect(names(sortNodes(list, "route", "desc"))).toEqual(["unknown", "relay", "direct"]);
  });

  it("版本按数值段比，不按字符串", () => {
    const list = [
      row({ ip: "10.0.0.1", hostname: "a", version: "2.10.0-bbb" }),
      row({ ip: "10.0.0.2", hostname: "b", version: "2.7.1-aaa" }),
      row({ ip: "10.0.0.3", hostname: "c", version: "2.9.9-ccc" }),
    ];
    expect(names(sortNodes(list, "version", "asc"))).toEqual(["b", "c", "a"]);
  });

  it("延迟未知的行正序倒序都排在最后", () => {
    const list = [
      row({ ip: "10.0.0.1", hostname: "unknown", latencyMs: null }),
      row({ ip: "10.0.0.2", hostname: "slow", latencyMs: 140 }),
      row({ ip: "10.0.0.3", hostname: "fast", latencyMs: 10 }),
    ];
    expect(names(sortNodes(list, "latency", "asc"))).toEqual(["fast", "slow", "unknown"]);
    expect(names(sortNodes(list, "latency", "desc"))).toEqual(["slow", "fast", "unknown"]);
  });

  it("同值行保持 core 给的相对顺序", () => {
    const list = [
      row({ ip: "10.0.0.1", hostname: "first", latencyMs: 10 }),
      row({ ip: "10.0.0.2", hostname: "second", latencyMs: 10 }),
      row({ ip: "10.0.0.3", hostname: "third", latencyMs: 10 }),
    ];
    expect(names(sortNodes(list, "latency", "asc"))).toEqual(["first", "second", "third"]);
  });

  it("比较器可直接用，且方向可翻转", () => {
    const a = row({ hostname: "node2" });
    const b = row({ hostname: "node10" });
    expect(compareNodes(a, b, "name", "asc")).toBeLessThan(0);
    expect(compareNodes(a, b, "name", "desc")).toBeGreaterThan(0);
  });
});
