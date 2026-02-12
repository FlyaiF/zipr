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

### 3.1 查看全部文件
```bash
./zipr list sb-pkg.jar
```

### 3.2 导出单个文件
```bash
./zipr get "sb-pkg.jar!/BOOT-INF/classes/application.properties" --out app.properties
```

### 3.3 删除单个文件
```bash
./zipr delete "sb-pkg.jar!/BOOT-INF/classes/application.properties"
```

### 3.4 替换单个文件
```bash
./zipr replace \
  "sb-pkg.jar!/BOOT-INF/classes/com/example/sbpkg/FirstPrinter.class" \
  FirstPrinter.class
```

### 3.5 对比两个压缩包（确认变更项）
```bash
./zipr diff old.jar new.jar
```

输出说明：
- `A`：新增文件
- `D`：删除文件
- `M`：修改文件（内容变化或元数据变化）

## 4. 批量替换（patch）
### 4.1 先生成草稿
```bash
./zipr patch draft sb-pkg.jar --from-dir ./patches -o patch.draft.toml
```

### 4.2 确认后执行
```bash
./zipr patch apply sb-pkg.jar --spec patch.draft.toml --dry-run
./zipr patch apply sb-pkg.jar --spec patch.draft.toml
```

## 5. 版本与排查
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

## 6. 安全说明
- 工具会先写临时文件，再原子替换原文件，尽量降低损坏风险。
- 建议先对原始文件做备份，再执行批量修改。
