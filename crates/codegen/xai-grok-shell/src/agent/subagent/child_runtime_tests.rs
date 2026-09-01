use super::*;

#[tokio::test]
async fn unfinished_thread_does_not_permit_resource_release() {
    let (finish, release) = std::sync::mpsc::channel();
    let thread = SessionThread::from_handle(std::thread::spawn(move || {
        let _ = release.recv();
    }));
    assert!(!await_session_thread_exit(&thread, std::time::Duration::ZERO).await);
    finish.send(()).unwrap();
    assert!(await_session_thread_exit(&thread, std::time::Duration::from_secs(5)).await);
    assert!(await_session_thread_exit(&thread, std::time::Duration::ZERO).await);
}

#[tokio::test]
async fn wait_does_not_block_local_async_work() {
    let (finish, release) = std::sync::mpsc::channel();
    let thread = SessionThread::from_handle(std::thread::spawn(move || {
        let _ = release.recv();
    }));
    let (exited, ()) = tokio::join!(
        await_session_thread_exit(&thread, std::time::Duration::from_secs(5)),
        async move {
            tokio::task::yield_now().await;
            finish.send(()).unwrap();
        },
    );
    assert!(exited);
}
