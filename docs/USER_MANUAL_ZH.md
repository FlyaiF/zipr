# zipr 用户手册（非编程人员）

本文用于随 `zipr` 可执行文件一起发布，面向日常运维与业务同学。

## 1. zipr 是什么
`zipr` 用来查看和修改压缩包中的文件，支持：
- `zip`
- `jar`
- `war`

并且支持“压缩包里还有压缩包”的场景。

## 2. 路径写法
当要定位压缩包内部文件时，使用 `!/`：

`外层.jar!/内层.jar!/文件路径`

示例：
`sb-pkg.jar!/BOOT-INF/lib/sb-modulea-0.0.1-SNAPSHOT.jar!/print.properties`

## 3. 常用操作
假设当前目录有 `zipr` 可执行文件。

## 3. 默认用法（最常用）

### 3.1 便捷模糊替换（传 2 个参数）
```bash
# 仅替换同名文件：自动匹配并直接执行
./zipr sb-pkg.jar ./patches/print.properties

# 批量目录替换：遇到多候选会打印 manifest 并询问确认
./zipr sb-pkg.jar ./patches/
```

行为说明：
- 如果源文件都能唯一匹配目标路径，直接替换并输出 `applied: replaced=X`。
- 若存在冲突（同名多处或未匹配），会打印 TOML 清单并提示 `[y/N]`；输入 `y`/`yes` 继续应用已匹配的部分，否则退出不改动。

### 3.2 查看全部文件（传 1 个参数）
```bash
./zipr list sb-pkg.jar
# 或直接
./zipr sb-pkg.jar
```

不带任何参数会打印帮助并返回非零状态，便于脚本检测。

## 4. 其他常用操作

### 4.1 导出单个文件
```bash
./zipr get "sb-pkg.jar!/BOOT-INF/classes/application.properties" --out app.properties
```

### 4.2 删除单个文件
```bash
./zipr delete "sb-pkg.jar!/BOOT-INF/classes/application.properties"
```

### 4.3 替换单个文件
```bash
./zipr replace \
  "sb-pkg.jar!/BOOT-INF/classes/com/example/sbpkg/FirstPrinter.class" \
  FirstPrinter.class
```

### 4.4 对比两个压缩包（确认变更项）
```bash
./zipr diff old.jar new.jar
```

输出说明：
- `A`：新增文件
- `D`：删除文件
- `M`：修改文件（内容变化或元数据变化）

## 5. 批量替换（patch）
### 5.1 先生成草稿
```bash
./zipr patch draft sb-pkg.jar --from-dir ./patches -o patch.draft.toml
```

### 5.2 确认后执行
```bash
./zipr patch apply sb-pkg.jar --spec patch.draft.toml --dry-run
./zipr patch apply sb-pkg.jar --spec patch.draft.toml
```

## 6. 版本与排查
查看版本和构建修订号（git revision）：
```bash
./zipr version
# 或
./zipr --version
```

反馈问题时请一并提供：
- 执行命令
- 报错信息
- `zipr version` 输出

## 7. 安全说明
- 工具会先写临时文件，再原子替换原文件，尽量降低损坏风险。
- 建议先对原始文件做备份，再执行批量修改。
