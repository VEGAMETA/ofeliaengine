# acl-compare

Research harness for ADR 0068: `engine_anim`'s clip format against the
[Animation Compression Library](https://github.com/nfrechette/acl) (ACL 2.1.0, MIT, Nicholas
Frechette; with [Realtime Math](https://github.com/nfrechette/rtm) 2.3.1, MIT) on the same
uniformly sampled clips of the Khronos sample models, the same bound and shell distances,
the error measured by one metric (object space, virtual vertices, at every sample and
between samples, against the glTF source).

ACL is not vendored. Clone both and point the build at them:

```sh
git clone --depth 1 --branch v2.1.0 https://github.com/nfrechette/acl.git
git clone --depth 1 --branch v2.3.1 https://github.com/nfrechette/rtm.git
ACL_DIR=acl RTM_DIR=rtm cargo run --release -- <test-assets dir>
```
