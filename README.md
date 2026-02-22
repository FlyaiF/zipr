# zipr 简易使用说明

## 1. 构建与帮助
```bash
cargo build
cargo run -- --help
```

## 2. 默认入口（最常用）

- 双参模糊替换：`zipr <archive> <file|dir>`
- 单参递归列出：`zipr <archive>`

### 双参模糊替换（默认优先）
传入压缩包路径 + 本地文件/目录时，自动生成并应用模糊匹配的替换清单：
```bash
# 单文件：匹配同名/同路径目标并直接替换
cargo run -- tests/assets/sb-pkg.jar patches/a.txt

# 目录：递归匹配，遇到多候选时会打印 manifest 并询问确认
cargo run -- tests/assets/sb-pkg.jar patches/
```
行为说明：
- 如果所有源文件都能唯一匹配目标，直接替换并输出 `applied: replaced=X` 摘要。
- 如果存在冲突（多候选或找不到目标），会打印 TOML manifest，提示 `[y/N]` 是否继续应用已匹配的部分；输入 `y`/`yes` 继续，否则退出不改动。

### 单参递归列出（仍为默认）
```bash
cargo run -- list tests/assets/sb-pkg.jar
# 或直接
cargo run -- tests/assets/sb-pkg.jar
```
不带任何参数会打印帮助并返回非零状态，便于脚本检测。

### 获取单个文件（支持嵌套 `!/`）
```bash
cargo run -- get "a.jar!/BOOT-INF/lib/b.jar!/x/y.txt" --out y.txt
```

### 删除单个文件
```bash
cargo run -- delete "a.jar!/BOOT-INF/classes/com/example/A.class"
```

### 替换单个文件
```bash
cargo run -- replace \
  "a.jar!/BOOT-INF/lib/b.jar!/com/example/B.class" \
  tests/assets/PrintFromModule.class
```

### 对比两个压缩包（递归 diff）
```bash
cargo run -- diff old.jar new.jar
```

## 3. patch 模式（精确批量替换）

### 生成草稿
```bash
cargo run -- patch draft a.jar --from-dir ./patches -o patch.draft.toml
```

### 先预演再应用
```bash
cargo run -- patch apply a.jar --spec patch.draft.toml --dry-run
cargo run -- patch apply a.jar --spec patch.draft.toml
```

## 4. 配置归档扩展名
默认支持：`zip,jar,war`。

可临时覆盖：
```bash
cargo run -- --archive-ext zip,jar,war,ear list a.ear
```

## 5. 注意事项
- 嵌套路径统一使用 `!/`，例如：`outer.jar!/inner.jar!/a.txt`。
- 修改操作会使用当前目录临时文件并原子替换，降低损坏风险。
- `method=inherit` 时会保留原条目的压缩方法（`STORED/DEFLATED`）。

## 6. 分发打包
```bash
cargo run --bin build_dist
```

打包结果在 `dist/`，包含：
- `zipr` 可执行文件
- `README.md`
- `USER_MANUAL_ZH.md`
- `patch.example.toml`
- `BUILD_INFO.txt`

可执行文件已注入 `git revision`，可通过以下命令查看：
```bash
./zipr version
# 或
./zipr --version
```
