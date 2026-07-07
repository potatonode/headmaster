# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0](https://github.com/potatonode/headmaster/compare/headmaster-v0.1.0...headmaster-v0.2.0) (2026-07-07)


### Features

* replace k8s-openapi-ext with workspace crate k8s-ext ([#18](https://github.com/potatonode/headmaster/issues/18)) ([b83ab40](https://github.com/potatonode/headmaster/commit/b83ab40500851f2152841b0aa34ed92ab2952a9a))
* **scim:** add /internal/reconcile endpoint and operator notification ([#28](https://github.com/potatonode/headmaster/issues/28)) ([7288f49](https://github.com/potatonode/headmaster/commit/7288f491a05de25b15cd45ff8bc9b0ad53b8c18e))
* **scim:** add PolicyUserKey, ExternalId mode, and expire_nodes_on_change ([#23](https://github.com/potatonode/headmaster/issues/23)) ([c5f1e60](https://github.com/potatonode/headmaster/commit/c5f1e60c43f75c6165dc3540bda1e9a4a823c29a))


### Bug Fixes

* **build:** pin operator to k8s-openapi v1_32 and decouple envtest from kube feature ([#26](https://github.com/potatonode/headmaster/issues/26)) ([6b787d7](https://github.com/potatonode/headmaster/commit/6b787d7f6823e9ea3161ed1fe0fc781ca5bd3962))
* **chart:** switch headscale image to Docker Hub, bump to v0.29.2 ([#46](https://github.com/potatonode/headmaster/issues/46)) ([216cc6a](https://github.com/potatonode/headmaster/commit/216cc6ad8e8cbf381c35bc6d530d3e2f7a10bb1c))
* **ci:** restrict release-please to root Cargo.toml; fix bypass actor app ID ([#49](https://github.com/potatonode/headmaster/issues/49)) ([485f452](https://github.com/potatonode/headmaster/commit/485f452f914f60ae1a88618cef6f0e94cf2b1019))
* **ci:** use task references in e2e-cleanup instead of shell subprocess ([#42](https://github.com/potatonode/headmaster/issues/42)) ([dbca336](https://github.com/potatonode/headmaster/commit/dbca33680ca19e8305185249c17626e8a9dbe4a4))
* **deps:** update rust crate shadow-rs to v2 ([#11](https://github.com/potatonode/headmaster/issues/11)) ([fd92616](https://github.com/potatonode/headmaster/commit/fd926167ca91aa2d1a27e359f7632a66af17436b))
* **deps:** update rust dependencies ([#7](https://github.com/potatonode/headmaster/issues/7)) ([774fee6](https://github.com/potatonode/headmaster/commit/774fee62c9a17caacd70a534943c4bad991a5d5f))
* helm dependency build examples/ before linting ([b471ea5](https://github.com/potatonode/headmaster/commit/b471ea51b394f07b2100330f776466a4eb686449))
* **ingress:** deregister node from old headscale when headscale-ref changes ([#37](https://github.com/potatonode/headmaster/issues/37)) ([66d3f5c](https://github.com/potatonode/headmaster/commit/66d3f5c27cb4e41d194634c4664da282627aded4))
* **ingress:** harden sharding gates, class-change release, and claim-default tristate ([#38](https://github.com/potatonode/headmaster/issues/38)) ([c4f625a](https://github.com/potatonode/headmaster/commit/c4f625aa7e95beeaca3123da1352c6c13c389d05))
* remove extra blank line in policy.rs (rustfmt) ([c60d3b7](https://github.com/potatonode/headmaster/commit/c60d3b79ebea5e4bc46cba8a78d7629e5749636f))


### Performance Improvements

* cargo-chef + GHA Docker layer cache for e2e builds ([13b872f](https://github.com/potatonode/headmaster/commit/13b872f63d93a80703dcf2fa37f4365ab59426a8))

## [Unreleased]

[Unreleased]: https://github.com/potatonode/headmaster/commits/main/
