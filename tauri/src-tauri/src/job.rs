//! 用 Job Object 管引擎的进程树：起的时候塞进去，停的时候一次收干净。
//! 搬自 Dockside 的 src-tauri/src/proc/job.rs，砍到只剩引擎用得上的部分。
//!
//! 取代原来的 `taskkill /pid N /t /f`。换掉的理由：
//!
//! - **关机时起不了新进程。** 会话开始拆了以后再 CreateProcess，loader 返回
//!   0xc0000142（STATUS_DLL_INIT_FAILED），系统弹一个 taskkill.exe 的应用程序
//!   错误框，而这个框本身又挡在「正在关闭 N 个应用」那一屏上，把重启卡住。
//!   `TerminateJobObject` 是进程内的一次系统调用，什么时候调都行。
//! - engine.py 下模型时生的子进程**自动继承 job 成员身份**，所以杀树不用再靠
//!   外部工具去遍历父子关系。
//! - 带上 `KILL_ON_JOB_CLOSE`：Localless 自己被任务管理器 / taskkill /F 强杀时，
//!   句柄跟着进程关，整棵引擎树一起走，不再留下攥着 8765 和显存的孤儿。
//!
//! spawn 到 assign 之间有几十微秒的窗口，python 在这段时间里还在做 DLL 初始化，
//! 轮不到它生孙子；取舍的完整说明见 Dockside 那份的文件头。

use std::ffi::c_void;
use std::mem::{size_of, zeroed};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

pub type Result<T> = windows::core::Result<T>;

pub struct Job(HANDLE);

// HANDLE 内部是裸指针所以默认不是 Send。job 句柄是内核对象，跨线程使用是合法的，
// 而且只在 engine 的 PROC 锁里碰它。
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    /// 建一个匿名 job，并打开「句柄关掉就杀光成员」。
    pub fn create() -> Result<Self> {
        // SAFETY: 传 None + 空名字建匿名对象，失败时 windows crate 已经转成 Err。
        let handle = unsafe { CreateJobObjectW(None, None)? };
        let job = Job(handle);

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // 刻意不设任何 breakaway 标志：子进程不许自己跳出 job，否则杀树就不完整了。

        // SAFETY: info 是本地变量，大小按类型算，类型和 information class 匹配。
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
        }
        Ok(job)
    }

    /// 把 `Command::spawn` 出来的进程塞进来。它之后生的所有后代自动也是成员。
    pub fn assign(&self, child: &std::process::Child) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: child 由我们持有、还没 wait 过，句柄有效；std spawn 出来的进程
        // 句柄是 PROCESS_ALL_ACCESS，满足 PROCESS_SET_QUOTA | PROCESS_TERMINATE。
        unsafe { AssignProcessToJobObject(self.0, HANDLE(child.as_raw_handle())) }
    }

    /// 一次杀光整棵树。
    pub fn terminate(&self) -> Result<()> {
        // SAFETY: self.0 在 Job 存活期间始终有效。
        unsafe { TerminateJobObject(self.0, 1) }
    }

    /// 还在 job 里的进程数。只有测试用得上。
    #[cfg(test)]
    fn active(&self) -> u32 {
        use windows::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        // SAFETY: 缓冲区是本地变量，大小如实上报。
        unsafe {
            QueryInformationJobObject(
                Some(self.0),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut c_void,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
        }
        .expect("查 job");
        info.ActiveProcesses
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE 在这里生效：最后一个句柄关掉，成员全部被杀。
        // 所以引擎跑着的时候 Job 必须一直跟 Child 放在一起，不能提前 drop。
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// cmd 是爹、ping 是孙子——正是 engine.py 生下模型子进程的那个形状。
    fn spawn_tree() -> std::process::Child {
        Command::new("cmd")
            .args(["/d", "/s", "/c", "ping -n 60 127.0.0.1"])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("起 cmd")
    }

    #[test]
    fn 孙子进程也进_job_且能一次杀光() {
        let job = Job::create().expect("建 job");
        let mut child = spawn_tree();
        job.assign(&child).expect("assign");

        assert!(wait_until(|| job.active() >= 2), "ping 没有进 job，杀树会漏");
        job.terminate().expect("terminate");
        assert!(wait_until(|| job.active() == 0), "terminate 之后还剩 {} 个", job.active());
        assert!(wait_until(|| child.try_wait().unwrap().is_some()), "cmd 还活着");
    }

    /// Localless 被强杀时靠的就是这一条：句柄一关，树跟着走。
    #[test]
    fn 丢掉_job_整棵树跟着走() {
        let job = Job::create().expect("建 job");
        let mut child = spawn_tree();
        job.assign(&child).expect("assign");
        assert!(wait_until(|| job.active() >= 2));

        drop(job);
        assert!(wait_until(|| child.try_wait().unwrap().is_some()), "job 关了 cmd 还活着");
    }
}
