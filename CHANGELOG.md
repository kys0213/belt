# Changelog

## [0.1.7](https://github.com/kys0213/belt/compare/v0.1.6...v0.1.7) (2026-03-30)


### Features

* **cli:** add claw workspace init validation to workspace add ([#558](https://github.com/kys0213/belt/issues/558)) ([932f8c2](https://github.com/kys0213/belt/commit/932f8c2298934753841ba2226f96e10edd817880)), closes [#546](https://github.com/kys0213/belt/issues/546)
* **cli:** add countdown display in belt status --format rich ([#648](https://github.com/kys0213/belt/issues/648)) ([af0c40d](https://github.com/kys0213/belt/commit/af0c40db11dae98f86a991c58d145fa2f2f47a87)), closes [#625](https://github.com/kys0213/belt/issues/625)
* **cli:** add R key for manual TUI dashboard refresh ([#634](https://github.com/kys0213/belt/issues/634)) ([731c287](https://github.com/kys0213/belt/commit/731c287306f70abcebf1f5c6e2fd3f38c697f94b)), closes [#624](https://github.com/kys0213/belt/issues/624)
* **cli:** complete bootstrap --create-pr implementation ([#603](https://github.com/kys0213/belt/issues/603)) ([5119369](https://github.com/kys0213/belt/commit/5119369086cdb663049303b11736ae3cae42b717)), closes [#580](https://github.com/kys0213/belt/issues/580)
* **cli:** complete spec lifecycle pipeline from decompose to collect ([#660](https://github.com/kys0213/belt/issues/660)) ([cad61d1](https://github.com/kys0213/belt/commit/cad61d182bbab2d342cf6bf41561af060389f546)), closes [#574](https://github.com/kys0213/belt/issues/574)
* **cli:** force trigger gap_detection on spec update ([#554](https://github.com/kys0213/belt/issues/554)) ([3c6c018](https://github.com/kys0213/belt/commit/3c6c018d53390a1db10fe8b98abf55fd48c657e4)), closes [#544](https://github.com/kys0213/belt/issues/544)
* **cli:** implement belt agent subcommands (init, rules, edit, plugin, context) ([#661](https://github.com/kys0213/belt/issues/661)) ([6cf64eb](https://github.com/kys0213/belt/commit/6cf64ebec1195bc4b4d6fda895016cecd0a86b41))
* **cli:** implement spec status linked issues and acceptance criteria display ([#604](https://github.com/kys0213/belt/issues/604)) ([f361056](https://github.com/kys0213/belt/commit/f361056714c699b6a8ac15b820cf54fec99d01ab)), closes [#490](https://github.com/kys0213/belt/issues/490)
* **cli:** implement spec verify, link, and unlink commands ([#557](https://github.com/kys0213/belt/issues/557)) ([3c75f05](https://github.com/kys0213/belt/commit/3c75f0596f774bcc59ae357d94c0507f0a6295d7)), closes [#545](https://github.com/kys0213/belt/issues/545)
* **cli:** make belt agent work without --workspace flag ([#657](https://github.com/kys0213/belt/issues/657)) ([df87220](https://github.com/kys0213/belt/commit/df87220b4567a5e9d615b8f1d2eff7b8c4ec3569)), closes [#618](https://github.com/kys0213/belt/issues/618)
* **daemon:** add TransitionEvent logging for on_enter, handler, and evaluate ([#698](https://github.com/kys0213/belt/issues/698)) ([5502254](https://github.com/kys0213/belt/commit/550225451ab29b317293852e8f0da347f5e031c1)), closes [#678](https://github.com/kys0213/belt/issues/678)
* **daemon:** connect gap-detection cron to issue creation pipeline ([#656](https://github.com/kys0213/belt/issues/656)) ([524b324](https://github.com/kys0213/belt/commit/524b3244aff4ebc09860b89edd40da8f3ed071b8)), closes [#573](https://github.com/kys0213/belt/issues/573)
* **dist:** add --yes flag, network check, and improved error handling to install.sh ([#556](https://github.com/kys0213/belt/issues/556)) ([5c27649](https://github.com/kys0213/belt/commit/5c276490408a308db3230a00c8923fa9ed645f7c)), closes [#549](https://github.com/kys0213/belt/issues/549)
* **status:** display token usage aggregation in status output ([#596](https://github.com/kys0213/belt/issues/596)) ([c02cc13](https://github.com/kys0213/belt/commit/c02cc13d27297dbfd47d509f08b491b84e836617)), closes [#575](https://github.com/kys0213/belt/issues/575)


### Bug Fixes

* **ci:** add skip-labeling to release-please to avoid GraphQL race condition ([#503](https://github.com/kys0213/belt/issues/503)) ([6ff8c21](https://github.com/kys0213/belt/commit/6ff8c21d032238c2fe6433e2bccd67bb4f8a11f1)), closes [#500](https://github.com/kys0213/belt/issues/500)
* **ci:** pin release-please-action to commit with release-please 17.3.0 ([#631](https://github.com/kys0213/belt/issues/631)) ([c6161ba](https://github.com/kys0213/belt/commit/c6161ba26c878f3373c98892d15025f2372168b7)), closes [#585](https://github.com/kys0213/belt/issues/585)
* **ci:** update release-please-action SHA to v4.4.0 ([#701](https://github.com/kys0213/belt/issues/701)) ([ea78733](https://github.com/kys0213/belt/commit/ea78733ff0baf86469b010fc4260acedd3b95f8e)), closes [#668](https://github.com/kys0213/belt/issues/668)
* **cli:** add worktree cleanup on Done/Skipped transition via CLI ([#659](https://github.com/kys0213/belt/issues/659)) ([1d673d2](https://github.com/kys0213/belt/commit/1d673d22ff0ed7aab86278af35f74f0727d84692)), closes [#495](https://github.com/kys0213/belt/issues/495)
* **cli:** belt queue done should trigger on_done script execution ([#658](https://github.com/kys0213/belt/issues/658)) ([e98d1d0](https://github.com/kys0213/belt/commit/e98d1d08c81ad490cb746e509ec7eaf419dfce27)), closes [#494](https://github.com/kys0213/belt/issues/494)
* **cli:** eliminate test race condition by passing belt_home explicitly ([#505](https://github.com/kys0213/belt/issues/505)) ([2f0486b](https://github.com/kys0213/belt/commit/2f0486bc406612db5f8380dc6c1fd42fd9c01879)), closes [#502](https://github.com/kys0213/belt/issues/502)
* **cli:** read trigger.label from workspace config instead of fixed autopilot:ready ([#647](https://github.com/kys0213/belt/issues/647)) ([2e9c5dc](https://github.com/kys0213/belt/commit/2e9c5dcdce47e952f45c1c2426d00a9e4cf5cd3d)), closes [#621](https://github.com/kys0213/belt/issues/621)
* **cli:** register Gemini and Codex runtimes in build_registry ([#601](https://github.com/kys0213/belt/issues/601)) ([6a1f0de](https://github.com/kys0213/belt/commit/6a1f0de66b2ebe378eff4972ad560ea604297926)), closes [#489](https://github.com/kys0213/belt/issues/489)
* **cli:** resolve type error in Windows terminate_pid block ([#504](https://github.com/kys0213/belt/issues/504)) ([2b1d667](https://github.com/kys0213/belt/commit/2b1d66718522ef0e2508ac340eae7ce9c96626ce)), closes [#501](https://github.com/kys0213/belt/issues/501)
* **cli:** update resolve_rules_dir Priority 3 path from claw-workspace to agent-workspace ([#655](https://github.com/kys0213/belt/issues/655)) ([7822530](https://github.com/kys0213/belt/commit/7822530aeb1e415e883258ebf15abfe9f98cfdb7)), closes [#623](https://github.com/kys0213/belt/issues/623)
* **cli:** use unique workspace names in resolve_rules_dir tests ([#632](https://github.com/kys0213/belt/issues/632)) ([96da573](https://github.com/kys0213/belt/commit/96da573c739c23f756dd19a34a6667a949be9751)), closes [#587](https://github.com/kys0213/belt/issues/587)
* **core:** add missing state field to IssueContext ([#595](https://github.com/kys0213/belt/issues/595)) ([a39288b](https://github.com/kys0213/belt/commit/a39288b95f38acfb8b88d4c04093582cf6441a0d)), closes [#497](https://github.com/kys0213/belt/issues/497)
* **core:** convert ShellExecutor and TestRunner to async traits ([#689](https://github.com/kys0213/belt/issues/689)) ([51d786c](https://github.com/kys0213/belt/commit/51d786cf5179794343ffa6b1a68e9dffa298452a)), closes [#614](https://github.com/kys0213/belt/issues/614)
* **core:** spec add auto-transitions to Active, allow Completed-&gt;Archived ([#593](https://github.com/kys0213/belt/issues/593)) ([f9226bb](https://github.com/kys0213/belt/commit/f9226bb60c85cec565ac731de4b2c165313953ed)), closes [#491](https://github.com/kys0213/belt/issues/491)
* **core:** verify context response includes full history field ([#613](https://github.com/kys0213/belt/issues/613)) ([#688](https://github.com/kys0213/belt/issues/688)) ([3321b48](https://github.com/kys0213/belt/commit/3321b482dfcc0d019d0ce86390e6e0b39be9a3c4))
* **daemon:** correct graceful shutdown rollback behavior on timeout ([#555](https://github.com/kys0213/belt/issues/555)) ([5940479](https://github.com/kys0213/belt/commit/5940479a256d453a303cc958edeb1a1f96e5959f)), closes [#543](https://github.com/kys0213/belt/issues/543)
* **daemon:** HITL respond 'done' should execute on_done script before transition ([#591](https://github.com/kys0213/belt/issues/591)) ([2556f89](https://github.com/kys0213/belt/commit/2556f89a3c6989f44c6777e0ec3e777c9e2b87eb))
* **daemon:** HITL timeout terminal 'replan' not properly handled ([#589](https://github.com/kys0213/belt/issues/589)) ([f556e4d](https://github.com/kys0213/belt/commit/f556e4d997eee7837ae39434daafbf7b4bc3ecd0))
* **daemon:** inject WORKSPACE and BELT_DB env vars in CustomScriptJob ([#633](https://github.com/kys0213/belt/issues/633)) ([1d9f909](https://github.com/kys0213/belt/commit/1d9f9090b5107d35f4b077aef5cabe3389d8c09b)), closes [#493](https://github.com/kys0213/belt/issues/493)
* **daemon:** replace auth stub functions with real implementation logic in gap_detection test ([#528](https://github.com/kys0213/belt/issues/528)) ([b94f2b3](https://github.com/kys0213/belt/commit/b94f2b33e03da7cf46d7843dda520243ed21caed)), closes [#510](https://github.com/kys0213/belt/issues/510)
* **daemon:** replace JSON verdict evaluate with CLI direct call ([#588](https://github.com/kys0213/belt/issues/588)) ([1b394e7](https://github.com/kys0213/belt/commit/1b394e76922461027d93ce38bb8fd1a9ca7b4d89))
* **daemon:** replan max exceeded should transition to Skipped, not Failed ([#590](https://github.com/kys0213/belt/issues/590)) ([f275f07](https://github.com/kys0213/belt/commit/f275f076e4078dd63aac9929f2321cd8464ee698)), closes [#487](https://github.com/kys0213/belt/issues/487)
* **daemon:** update force_fail_running error message to reflect second SIGINT context ([#592](https://github.com/kys0213/belt/issues/592)) ([ead7f3d](https://github.com/kys0213/belt/commit/ead7f3d8fc297441488639cf5399c980accf2221)), closes [#484](https://github.com/kys0213/belt/issues/484)
* **dist:** install.sh --yes flag auto-adds PATH to shell profile ([#594](https://github.com/kys0213/belt/issues/594)) ([3f0fa1e](https://github.com/kys0213/belt/commit/3f0fa1e89244e035c86e054fa1ac84d0c87734a5)), closes [#492](https://github.com/kys0213/belt/issues/492)
* **dist:** redirect installer say() output to stderr ([#479](https://github.com/kys0213/belt/issues/479)) ([6d40754](https://github.com/kys0213/belt/commit/6d407548fa85f19ef1320b529efdcd3c42236096))
* **infra:** enforce scan_interval_secs config in GitHubDataSource ([#635](https://github.com/kys0213/belt/issues/635)) ([db17c11](https://github.com/kys0213/belt/commit/db17c118120681e513a742658afce3707326c3b9)), closes [#620](https://github.com/kys0213/belt/issues/620)
* **infra:** include reviews field in fetch_linked_pr gh CLI query ([#636](https://github.com/kys0213/belt/issues/636)) ([508d163](https://github.com/kys0213/belt/commit/508d163d28f58cae19f42ab0d894aa8d98e14cae)), closes [#622](https://github.com/kys0213/belt/issues/622)
* **infra:** persist previous_worktree_path to database for worktree reuse ([#696](https://github.com/kys0213/belt/issues/696)) ([a4f42a1](https://github.com/kys0213/belt/commit/a4f42a1aa1ef709daa45586d42ad95f65806ff99)), closes [#676](https://github.com/kys0213/belt/issues/676)

## [0.1.6](https://github.com/kys0213/belt/compare/v0.1.5...v0.1.6) (2026-03-28)


### Bug Fixes

* **ci:** pass tag_name from release-please to release build workflow ([#477](https://github.com/kys0213/belt/issues/477)) ([cdffefc](https://github.com/kys0213/belt/commit/cdffefcf65a548115d06494f7a49ebf4c3f87110))

## [0.1.5](https://github.com/kys0213/belt/compare/v0.1.4...v0.1.5) (2026-03-28)


### Bug Fixes

* **ci:** chain release build from release-please via workflow_call ([#475](https://github.com/kys0213/belt/issues/475)) ([91a9181](https://github.com/kys0213/belt/commit/91a9181e964b240f6a2e1b1ba305e12f14ab599c))

## [0.1.4](https://github.com/kys0213/belt/compare/v0.1.3...v0.1.4) (2026-03-28)


### Bug Fixes

* **ci:** add push trigger with paths filter for release tag creation ([#473](https://github.com/kys0213/belt/issues/473)) ([e7a7409](https://github.com/kys0213/belt/commit/e7a7409d7d723c7f1696b36cbdbe3481b46b0d80))

## [0.1.3](https://github.com/kys0213/belt/compare/v0.1.2...v0.1.3) (2026-03-28)


### Bug Fixes

* **ci:** add release event trigger for Release Please compatibility ([#469](https://github.com/kys0213/belt/issues/469)) ([00357a2](https://github.com/kys0213/belt/commit/00357a292bd487ee4dd9c9f6ae141d177af8a0b4))
* **dist:** move _tmpdir to global scope for trap cleanup ([#471](https://github.com/kys0213/belt/issues/471)) ([5676d91](https://github.com/kys0213/belt/commit/5676d91de5c7352a0efd230e2073513b393d308d))

## [0.1.2](https://github.com/kys0213/belt/compare/v0.1.1...v0.1.2) (2026-03-28)


### Features

* **dist:** add install.sh and install.ps1 installer scripts ([#466](https://github.com/kys0213/belt/issues/466)) ([6d8fe4e](https://github.com/kys0213/belt/commit/6d8fe4e39adbf50ccc4f7affe3560e955ce21008))


### Bug Fixes

* **ci:** use simple release type for workspace compatibility ([#467](https://github.com/kys0213/belt/issues/467)) ([8f9af41](https://github.com/kys0213/belt/commit/8f9af41ddf190927a3dd097a046b9527c401427b))
