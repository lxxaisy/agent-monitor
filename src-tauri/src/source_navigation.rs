use std::path::Path;

fn zcode_project_url(path: &str) -> Result<reqwest::Url, String> {
    if !Path::new(path).is_absolute() {
        return Err("ZCode 项目路径必须是来源提供的绝对路径".into());
    }
    let mut url =
        reqwest::Url::parse("zcode://workspace/open").map_err(|error| error.to_string())?;
    url.query_pairs_mut().append_pair("path", path);
    Ok(url)
}

pub fn open_zcode_project(path: &str) -> Result<(), String> {
    let url = zcode_project_url(path)?;
    let path = Path::new(path);
    if !path.is_dir() {
        return Err(format!("ZCode 项目目录不存在：{}", path.display()));
    }
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/usr/bin/open")
            .args(["-b", "dev.zcode.app", "--"])
            .arg(url.as_str())
            .status()
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(format!("ZCode 未能打开项目：{status}"));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        Err("当前平台尚未提供 ZCode 桌面项目跳转".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zcode_url_opens_the_exact_project_and_does_not_invent_a_session_link() {
        let path = "/Users/example/项目 space & work#1";
        let url = zcode_project_url(path).unwrap();
        assert_eq!(url.scheme(), "zcode");
        assert_eq!(url.host_str(), Some("workspace"));
        assert_eq!(url.path(), "/open");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("path".into(), path.into())]
        );
        assert!(zcode_project_url("relative/project").is_err());
        assert!(zcode_project_url("").is_err());
    }
}
