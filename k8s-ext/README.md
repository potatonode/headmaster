# k8s-ext

Fluent builder and accessor extension traits for
[`k8s-openapi`](https://crates.io/crates/k8s-openapi) resource types.

## What it provides

**Extension traits** — one per resource or sub-resource type — expose a builder-style
API for constructing `k8s-openapi` structs without touching `Option` fields directly.
Each trait is implemented for the corresponding `k8s-openapi` type and re-exported from
the crate root.

| Trait                                | Covers                                              |
| ------------------------------------ | --------------------------------------------------- |
| `ResourceBuilder`                    | `namespace` and `labels` setters for any `Metadata` |
| `ConfigMapExt`                       | `ConfigMap`                                         |
| `ConfigMapVolumeSourceExt`           | `ConfigMapVolumeSource`                             |
| `ContainerExt`                       | `Container`                                         |
| `ContainerPortExt`                   | `ContainerPort`                                     |
| `EnvVarExt`, `ToEnvVar`, `ToEnvFrom` | `EnvVar`, `EnvVarSource`                            |
| `JobExt`                             | `Job`                                               |
| `PodSpecExt`                         | `PodSpec`                                           |
| `PodTemplateSpecExt`                 | `PodTemplateSpec`                                   |
| `PolicyRuleExt`                      | `PolicyRule`                                        |
| `ProbeExt`                           | `Probe`                                             |
| `RoleExt`                            | `Role`                                              |
| `RoleBindingExt`, `IsRole`           | `RoleBinding`                                       |
| `SecretExt`                          | `Secret`                                            |
| `SecretEnvSourceExt`                 | `SecretEnvSource`                                   |
| `SecretGetExt`                       | Reading typed values out of a `Secret`              |
| `ServiceExt`                         | `Service`                                           |
| `ServiceAccountExt`                  | `ServiceAccount`                                    |
| `ServicePortExt`                     | `ServicePort`                                       |
| `StatefulSetExt`                     | `StatefulSet`                                       |
| `StatefulSetGetExt`                  | Reading typed values out of a `StatefulSet`         |
| `SubjectExt`                         | `Subject`                                           |
| `VolumeExt`                          | `Volume`                                            |
| `VolumeMountExt`, `ToVolumeName`     | `VolumeMount`                                       |

**`label` module** — shared label key/value constants used across the workspace.

**`ToIntOrString`** — converts `i32`, `u16`, `&str`, and `String` to
`k8s-openapi`'s `IntOrString`.
