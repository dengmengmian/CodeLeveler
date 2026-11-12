先给结论：**能不能登，取决于三件事**，代码里都已经写死了判据。

## 导入与校验的实际逻辑

- 密码哈希是**原值搬运**：`backend/cmd/legacyimport/apply.go:376` 把旧库 `password_hash` 直接写进 `account.password_hash`，不做任何转换。
- 新系统**只认 bcrypt**：`backend/internal/core/password/password.go:16` 就是 `bcrypt.CompareHashAndPassword`。如果旧 Java 系统用的是 Spring Security 默认的 BCrypt（`$2a$10$...`），原样就能过；如果是 MD5/SHA/自定义加密，**这些账号一个都登不上**，报的会是「手机号或密码错误」。
- 登录入口按 **手机号 + 账号域** 查人：`session.go:88` → `repo/account.go:89` 的 `realm = ? AND phone = ?`。旧库的 `username` 字段压根没被读（`legacy.go:34` 的 `legacyUser` 结构里没有它）。所以这里说的"用户名"实际上是**手机号**。
- 账号状态也一起搬了：旧 `status=0` 映射成新 `status=2`（禁用，`plan.go:291`），这类账号即使密码正确也会被 `account.go:104` 的 `Enabled()` 挡掉。旧 `phone` 为空的用户在导入阶段就报错了（`plan.go:241`），不会进库。

## 四种验证办法（从快到慢）

**① 先离线看哈希格式，不用起服务**
```bash
grep -c '"password_hash":"\$2[aby]\$' users.jsonl   # bcrypt 条数
grep -c '"password_hash":"'              users.jsonl   # 有哈希的总条数
grep -o '"password_hash":"[^"]*"' users.jsonl | cut -c1-20 | sort | uniq -c | head
```
两个数相等 = 全是 bcrypt，登录大概率没问题；出现 `$1$`、32 位十六进制之类 = 格式不兼容，直接判定登不上。

**② 拿一个已知明文离线比对**（知道某个旧用户原密码时最省事）
```bash
cd backend
cat > /tmp/checkhash.go <<'EOF'
package main

import (
	"fmt"
	"os"

	"golang.org/x/crypto/bcrypt"
)

func main() {
	err := bcrypt.CompareHashAndPassword([]byte(os.Args[1]), []byte(os.Args[2]))
	fmt.Println("match:", err == nil)
}
EOF
go run /tmp/checkhash.go '<旧库里的 password_hash 原串>' '<你记得的明文>'
```
这一步不连库、不需要账号可用，纯粹验证哈希本身。

**③ 端到端真登录**（最贴近用户实际体验）
起本地环境后导入 dev 库，然后打 API：
```bash
make up   # dev 配置连 127.0.0.1:5432/devorder

curl -s -X POST http://127.0.0.1:8080/auth/CUSTOMER/login \
  -H 'Content-Type: application/json' \
  -d '{"phone":"13800138000","password":"[REDACTED]"}'
```
返回里带 `access_token` 就是成功。注意 **realm 不能错**：`CUSTOMER`（发单侧）和 `PROVIDER`（接单侧）是两个独立账号域，用错域会走「账号不存在」分支，表现为密码错误。旧系统 `role_type=2` 或 `role_type=0,user_type=2` 导成 `PROVIDER`，其余导成 `CUSTOMER`（见 README 映射表）。

**④ 直接查库核对导入结果**
```sql
SELECT account_no, realm, phone, status, left(password_hash, 7) AS hash_prefix
FROM account
WHERE account_no LIKE 'LEGACY-%'
ORDER BY id DESC
LIMIT 20;
```
看三样：`hash_prefix` 是不是 `$2a$10$`、`status` 是不是 `1`、`phone` 是否正确。`account_no` 形如 `LEGACY-C-000123` / `LEGACY-P-000123`，C/P 就对应 CUSTOMER/PROVIDER。

## 两个容易踩的坑

1. **账号存在 ≠ 能登录**：`status=2` 的账号密码对了也返回禁用错误，和第①步的哈希判断要看两个维度。
2. **重复导入不会覆盖哈希**：`upsertAccount` 用的是 `ON CONFLICT (account_no) DO NOTHING`（`apply.go:381`），如果之前已经导过一次、后来旧库改了密码，再跑一遍**不会更新**已有账号的 `password_hash`，必须手工改库或先删再导。

## 宽度与字符边界

混合行：中文、Latin、emoji 🚀🧪 与组合字符 é、全角符号ＡＢＣ、窄字符 iiii 连续出现。

- 宽字符「界」后紧跟窄字符 `abc`，再跟组合序列 `é`；
- 一行里同时出现 `→`、`①`、`™` 与 emoji 👩‍💻，用于检查列宽推进。

另外确认一下你要验的库是哪个：`config.dev.yaml` 指向本地 `127.0.0.1:5432/devorder`，`config.test.yaml` / `config.local.yaml` 指向远端 `203.0.113.10` 的 `devorder-test`。第①步只读导出文件，任何环境都能先跑，建议先做这一步再决定要不要起服务。