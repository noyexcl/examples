/// Bulding a middleware from scratch
/// https://github.com/tower-rs/tower/blob/master/guides/building-a-middleware-from-scratch.md
use pin_project::pin_project;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Sleep;
use tower::Service;

fn main() {}

/// `Service::call()` に渡された `&mut self` を独自の `self` に変換し、response future に
/// move 出来るようにするために、サービスが Clone を実装することは重要
///
/// `S` が `Service` を実装することを予期しているにも関わらず、トレイト境界を構造体に書かないのは
/// [Rust の API ガイドライン](https://rust-lang.github.io/api-guidelines/future-proofing.html#c-struct-bounds)
/// に従っているため
#[derive(Debug, Clone)]
struct Timeout<S> {
    inner: S,
    timeout: Duration,
}

impl<S> Timeout<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

impl<S, Request> Service<Request> for Timeout<S>
where
    S: Service<Request>,
    S::Error: Into<BoxError>, // 内側のサービスのエラーをトレイトオブジェクトに変換するために必要な制約
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // このミドルウェアはバックプレッシャーを気にしない
        // よって内側のサービスが ready なら ready
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // タイムアウトを実装するには、response future と指定時間後に完了する tokio::time::sleep の
        // future の2つを用意し、最初に完了した方を取るというアプローチを使う
        let response_future = self.inner.call(request);
        let sleep = tokio::time::sleep(self.timeout);

        // この関数の考えられる戻り値の型の一つは Pin<Box<dyn Future<...>>> である
        // しかし、巨大なスタックを持っていて、入れ子にされた十数のサービスの各層が、リクエストが通るたびに
        // 新しい Box の割り当てを行うとするとパフォーマンスに影響が出かねない
        // そこで Box の使用を避けるため、独自の Future の実装、ResponseFuture を書く
        ResponseFuture {
            response_future,
            sleep,
        }
    }
}

// 内側のサービスの response future の型に対してジェネリックである必要がある
// つまり F は self.inner.call(request) の型である
// これはサービスを別のサービスでラップすることと似ているが、ここでは future を他の future でラップしている
//
// pin_project と pin の意味は poll() 内のコメントを参照
#[pin_project]
pub struct ResponseFuture<F> {
    #[pin]
    response_future: F,
    #[pin]
    sleep: Sleep,
}

impl<F, Response, Error> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response, Error>>,
    Error: Into<BoxError>, //  内側のサービスのエラーをトレイトオブジェクトにするために必要な制約
{
    type Output = Result<Response, BoxError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 理想的には以下のようなものを書きたい
        // 1. self.response_future を poll して ready なら、その response かエラーを返す
        // 2. そうでなければ self.sleep を poll して ready なら、エラーを返す
        // 3. どちらでもない場合 Poll::Pending を返す
        //
        // match self.response_future.poll(cx) {
        //    Poll::Ready(result) => return Poll::Ready(result),
        //    Poll::Pending => {}
        // }
        //
        // しかしこれは動かない。poll は self が Pin<&mut Self> であることを要求するが
        // self.response_future は pin ではないからである
        //
        // ここで必要なのは pin projection と呼ばれるもので Pin<&mut Struct> から Pin<&mut Field> に
        // 進むことを意味する
        // pin projection は通常は unsafe なコードを書く必要があるが pin-project クレートは
        // それらを変わりにやってくれる

        // #[pin_project]によって生成された魔法の関数
        // project() は __ResponseFutureProjection 型を返し、ResponseFuture と同じフィールドを持っているが
        // #[pin] でアノテートされたフィールドを pin になるようにする
        let this = self.project();

        // 晴れて pin の response_future() を得られた
        let response_future: Pin<&mut F> = this.response_future;

        // また sleep() も pin である
        let sleep: Pin<&mut Sleep> = this.sleep;

        // もし #[pin] でアノテートされていないフィールドを持っていたとしたら、それは普通の &mut になる

        // 仮にここでエラーを返すとしたら、内側のサービスのエラーとこのサービスでのエラーの2種類が存在するのだが
        // 戻り値の型はどのように表現したら良いか？という問題がある
        //
        // ここではエラーを両方とも std::errror::Error のトレイトオブジェクトにすることで解決している
        // 従って ResponseFuture, Service の Error型変数 に Into<BoxError> という制約を付ける必要がある
        // この方法の利点やその他の方法については補足1を参照
        match response_future.poll(cx) {
            Poll::Ready(result) => {
                // 内側のサービスのエラー
                let result = result.map_err(Into::into);
                return Poll::Ready(result);
            }
            Poll::Pending => {}
        }

        match sleep.poll(cx) {
            Poll::Ready(()) => {
                // このサービスでのエラー
                let err = Box::new(TimeoutError(()));
                return Poll::Ready(Err(err));
            }
            Poll::Pending => {}
        }

        Poll::Pending
    }
}

// プライベートフィールド () を入れることで、このクレートの外のユーザが独自の TimeoutError を構築することを防いでいる
#[derive(Debug, Default)]
pub struct TimeoutError(());

impl fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("request timed out")
    }
}

impl std::error::Error for TimeoutError {}

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/*
--- 補足1: Error 型 ---
Service は内側のサービスで生じたエラーをそのまま返す必要があるが、内側のサービス自体がジェネリックになっているので
そのエラー型については何も知らず、その型を構築することも出来ない
つまり、外側のサービスで生じたエラーを内側のサービスのエラーの型と一致させて返すことが出来ない

この解決法は3つの選択肢がある

1. Box<dyn std::error::Error + Send + Sync> のように box されたエラートレイトオブジェクトを返す
2. サービスのエラーなのかタイムアウトのエラーなのかを列挙した enum を返す
3. TimeoutError 構造体を定義し、ジェネリックの Error 型が、TimeoutError::Into<Error>を使って、TimeoutErrorから
構築出来るように求める

3は最も柔軟性が高いように見えるが、ユーザー定義のエラー型が手動で From<TimeoutError> for MyError と実装しなくてはならないので
それぞれが独自のエラー型を持つミドルウェアを多く使うとすぐに冗長になる

2はエラー型の情報を失わないので表面上は理想的に見える

enum TimeoutError<Error> {
    Timeout(InnerTimeoutError),
    Service(Error)
}

しかし、このアプローチは3つの問題を抱えている

1. 実践では多くのミドルウェアを入れ子にすることが普通である。最終的なエラー列挙体がとても大きくなり
BufferError<RateLimitError<TimeoutError<MyError>>> のような形になるのも珍しくない。そのような型のパターンマッチングは
とても冗長である

2. ミドルウェアが適用される順番を変えると、最終的なエーラ型も変えることになるのでパターンマッチングも更新する必要がある

3. 最終的なエラー型がとても大きくなり、非常に大きなスタックのスペースを占める可能性がある

従って残された選択肢は1となる。これには3つの利点がある

1. ミドルウェアの順序を変えても最終的なエラー型が変わらないため、エラーハンドリングが壊れづらい
2. どれだけミドルウェアを積んでもエラー型のサイズは一定である
3. エラーを取り出すのに巨大な match を使う必要はなく、error.downcast_ref::<Timeout>() で良い

欠点は以下の2つ

1. 動的なダウンキャストを行うのでコンパイラはあらゆる可能性のあるエラー型に対してチェックしたと保証することは出来ない
2. エラーを作るのにアロケーションが必要になる。ただしこれは頻繁には起こらないと想定しているので、問題にはならない
*/
