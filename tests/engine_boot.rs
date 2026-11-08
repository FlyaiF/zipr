//! Runnable, generated Spring Boot 2.7/Java 8 and 3.5/Java 17 fixtures. No binary fixture is committed.
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::Command,
};
use zipr_lib::engine::*;
#[test]
#[ignore = "requires Java 8+, Maven, and Maven Central on first run"]
fn generated_boot_loader_starts_after_nested_class_patch()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    generated_boot("2.7.18", false)
}
#[test]
#[ignore = "requires JDK 17, Maven, and Maven Central on first run"]
fn generated_boot35_nestedjarfile_starts_after_nested_class_patch()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    generated_boot("3.5.0", true)
}
fn generated_boot(
    version: &str,
    modern: bool,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let home = PathBuf::from(std::env::var("HOME")?);
    let java_home = std::env::var_os(if modern { "JAVA17_HOME" } else { "JAVA_HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if modern {
                home.join("Library/Java/JavaVirtualMachines/openjdk-17.0.2/Contents/Home")
            } else {
                home.join(".local/sdks/java")
            }
        });
    let mvn = std::env::var_os("MAVEN_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/sdks/maven/bin/mvn"));
    let loader=home.join(format!(".m2/repository/org/springframework/boot/spring-boot-loader/{version}/spring-boot-loader-{version}.jar"));
    if !loader.exists() {
        let status = Command::new(mvn)
            .env("JAVA_HOME", &java_home)
            .args([
                "-q",
                "org.apache.maven.plugins:maven-dependency-plugin:3.6.1:get",
                &format!("-Dartifact=org.springframework.boot:spring-boot-loader:{version}"),
                "-Dtransitive=false",
            ])
            .status()?;
        assert!(status.success(), "Maven could not fetch Boot loader");
    }
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("demo");
    fs::create_dir(&src)?;
    let classes = dir.path().join("classes");
    fs::create_dir(&classes)?;
    fs::write(
        src.join("Main.java"),
        r#"package demo; public class Main { public static void main(String[] args) throws Exception {
        System.out.println(Message.text());
        if (Boolean.getBoolean("expect.nested")) {
            java.net.JarURLConnection c=(java.net.JarURLConnection)Message.class.getResource("Message.class").openConnection();
            if (!Class.forName("org.springframework.boot.loader.jar.NestedJarFile").isInstance(c.getJarFile())) throw new AssertionError("not NestedJarFile");
            System.out.println("NestedJarFile verified");
        }
    } }"#,
    )?;
    fs::write(
        src.join("Message.java"),
        "package demo; public class Message { public static String text() { return \"before\"; } }",
    )?;
    assert!(
        Command::new(java_home.join("bin/javac"))
            .arg("-d")
            .arg(&classes)
            .arg(src.join("Main.java"))
            .arg(src.join("Message.java"))
            .status()?
            .success()
    );
    let lib = dir.path().join("lib.jar");
    let mut z = zip::ZipWriter::new(fs::File::create(&lib)?);
    z.start_file(
        "demo/Message.class",
        zip::write::SimpleFileOptions::default(),
    )?;
    z.write_all(&fs::read(classes.join("demo/Message.class"))?)?;
    z.finish()?;
    let root = dir.path().join("boot.jar");
    let mut z = zip::ZipWriter::new(fs::File::create(&root)?);
    let mut loader = zip::ZipArchive::new(fs::File::open(loader)?)?;
    for i in 0..loader.len() {
        let mut f = loader.by_index(i)?;
        if f.name().starts_with("META-INF/") || f.is_dir() {
            continue;
        }
        z.start_file(f.name(), zip::write::SimpleFileOptions::default())?;
        std::io::copy(&mut f, &mut z)?;
    }
    z.start_file(
        "META-INF/MANIFEST.MF",
        zip::write::SimpleFileOptions::default(),
    )?;
    let launcher = if modern {
        "org.springframework.boot.loader.launch.JarLauncher"
    } else {
        "org.springframework.boot.loader.JarLauncher"
    };
    z.write_all(
        format!(
            "Manifest-Version: 1.0\r\nMain-Class: {launcher}\r\nStart-Class: demo.Main\r\n\r\n"
        )
        .as_bytes(),
    )?;
    if modern {
        z.start_file("META-INF/BOOT.SF", zip::write::SimpleFileOptions::default())?;
    }
    z.add_directory(
        "BOOT-INF/classes/",
        zip::write::SimpleFileOptions::default(),
    )?;
    z.start_file(
        "BOOT-INF/classes/demo/Main.class",
        zip::write::SimpleFileOptions::default(),
    )?;
    z.write_all(&fs::read(classes.join("demo/Main.class"))?)?;
    z.start_file(
        "BOOT-INF/lib/owned.jar",
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )?;
    z.write_all(&fs::read(lib)?)?;
    z.finish()?;
    let run = || {
        Command::new(java_home.join("bin/java"))
            .arg(format!("-Dexpect.nested={modern}"))
            .arg("-jar")
            .arg(&root)
            .output()
    };
    let output = run()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("before"));
    fs::write(
        src.join("Message.java"),
        "package demo; public class Message { public static String text() { return \"after patch\"; } }",
    )?;
    assert!(
        Command::new(java_home.join("bin/javac"))
            .arg("-d")
            .arg(&classes)
            .arg(src.join("Message.java"))
            .status()?
            .success()
    );
    apply(
        &Plan {
            root: root.clone(),
            ops: vec![Op::Replace {
                target: Chain::from("BOOT-INF/lib/owned.jar!/demo/Message.class"),
                source: classes.join("demo/Message.class"),
            }],
        },
        &ApplyOptions::default(),
        &Cache::new()?,
        &|_| {},
        &CancelToken::default(),
    )?;
    let output = run()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("after patch"));
    if modern {
        assert!(String::from_utf8_lossy(&output.stdout).contains("NestedJarFile verified"));
    }
    let mut b = Vec::new();
    Cache::new()?
        .read_entry(
            &root,
            &Chain::from("BOOT-INF/lib/owned.jar!/demo/Message.class"),
        )?
        .read_to_end(&mut b)?;
    assert_eq!(
        zipr_lib::inspect::class_info(&b)?.internal_name,
        "demo/Message"
    );
    Ok(())
}
