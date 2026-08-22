# CodeLeveler Mobile 冻结说明

**Status:** FROZEN  
**Tag:** `mobile-beta-mvp`  
**范围:** `apps/leveler-mobile` 的**功能投入**，不是整个 CodeLeveler、也不是 `leveler remote` 协议 1.0。

> Mobile Beta MVP 已收口。下一轮投入等真实用户用过之后再决定。  
> 在此之前：不开 Push、不开 Multi-Agent Fleet、不开 Voice、不重写客户端。

对照：[`MOBILE_BETA_CLOSURE.zh-CN.md`](MOBILE_BETA_CLOSURE.zh-CN.md)（本阶段做了什么）、[`MOBILE_RUNTIME_ALIGNMENT.zh-CN.md`](MOBILE_RUNTIME_ALIGNMENT.zh-CN.md)（设计）。

---

## 冻结的是什么

用户能走完这条闭环（登记过的产物才能预览）：

```text
手机创建任务
      ↓
Agent 执行
      ↓
Timeline 同步
      ↓
手机干预（steer）
      ↓
attachment_added → fetch_attachment
      ↓
预览结果
```

**明确不做、现在也不排期：**

- Push 通知
- Multi-Agent Fleet UI
- Voice Steering
- Remote Workspace / 手机 IDE
- 系统 Share sheet
- Agent `write_file` 自动登记为 attachment（host 缺口，见收口报告）
- 换 Flutter / 换导航框架 / 重写配对栈

## 冻结期间允许改什么

只修**已经承诺的闭环**被破坏的问题：

- 配对 / 验签 / relay / 会话流无法连上
- 审批、发消息、steer 发不出去或发错命令
- `fetch_attachment` 安全漏洞（路径穿越、未验签、公开 URL）
- 崩溃、数据丢失、把只读配对变成可写

这些也要最小 diff，并说明它对应哪一条已冻结行为。

**不允许：** 新屏幕、新 RPC、新事件类型「先做着」、把 Timeline 再做成聊天。

## 何时解冻

有真实 Beta 用户用过 `mobile-beta-mvp`，并且能回答：

1. 他们能不能靠手机把一个任务跑完并看到产物？
2. 卡在哪一步（配对、审批、steer、产物登记、下载）？
3. 下一轮投入值不值得？

没有这三份观察，不要开 Mobile 下一阶段。

## 和 STABILITY.md 的关系

`leveler remote` CLI / 配对载荷 / `~/.leveler/remote/` 在 [`STABILITY.md`](STABILITY.md) 里仍是 **Unstable**。  
这次冻结的是 **Flutter 客户端功能投入**，不是远程协议永不变更的承诺。协议若必须改，先写清是否迫使已配对设备重新配对，再改；不要借机加 Mobile 功能。
