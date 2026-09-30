//! FTP 认证与授权：自定义 `Authenticator`（对齐 Apache 行为）+ 只读授权用户。

use std::fmt::{Display, Formatter};

use async_trait::async_trait;
use unftp_core::auth::{
    AuthenticationError, Authenticator, Credentials, Principal, UserDetail, UserDetailError,
    UserDetailProvider,
};
use unftp_sbe_restrict::{UserWithPermissions, VfsOperations};

/// 会话用户：所有用户共享服务器根目录（`home()` 为 None，根目录 jail
/// 由 `Filesystem` 的 cap_std 沙箱接管），仅携带写权限位。
#[derive(Debug)]
pub struct FtpUser {
    pub username: String,
    pub writable: bool,
}

impl Display for FtpUser {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "FtpUser({})", self.username)
    }
}

impl UserDetail for FtpUser {}

impl UserWithPermissions for FtpUser {
    fn permissions(&self) -> VfsOperations {
        if self.writable {
            VfsOperations::all()
        } else {
            // 关闭全部写操作（STOR/DELE/MKD/RMD/RENAME），保留 GET/LIST/MD5。
            VfsOperations::all() - VfsOperations::WRITE_OPS
        }
    }
}

/// 把认证身份转换为会话用户；写权限来自服务器配置（匿名/具名一致，
/// 对齐原实现「writable 对所有用户生效」）。
#[derive(Debug)]
pub struct FtpUserProvider {
    pub writable: bool,
}

#[async_trait]
impl UserDetailProvider for FtpUserProvider {
    type User = FtpUser;

    async fn provide_user_detail(&self, principal: &Principal) -> Result<FtpUser, UserDetailError> {
        Ok(FtpUser {
            username: principal.username.clone(),
            writable: self.writable,
        })
    }
}

/// 对齐 Apache FTPServer 的认证语义：
///
/// - **匿名模式**（配置的用户名为空）：仅 `USER anonymous` 可登录，密码任意
///   （Apache 只注册了 anonymous 用户；密码存空串表示不校验，客户端惯例
///   发邮箱地址）。
/// - **具名模式**：用户名精确匹配且密码明文相等，否则拒绝
///   （对齐 PropertiesUserManager 的明文比对）。
#[derive(Debug)]
pub struct FtpAuth {
    pub username: String,
    pub password: String,
}

#[async_trait]
impl Authenticator for FtpAuth {
    async fn authenticate(
        &self,
        username: &str,
        creds: &Credentials,
    ) -> Result<Principal, AuthenticationError> {
        if self.username.is_empty() {
            return match username {
                "anonymous" => Ok(Principal {
                    username: username.to_string(),
                }),
                _ => Err(AuthenticationError::BadUser),
            };
        }
        if username != self.username {
            return Err(AuthenticationError::BadUser);
        }
        if creds.password.as_deref() == Some(self.password.as_str()) {
            Ok(Principal {
                username: username.to_string(),
            })
        } else {
            Err(AuthenticationError::BadPassword)
        }
    }
}
