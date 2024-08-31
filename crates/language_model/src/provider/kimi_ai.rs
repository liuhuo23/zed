use anyhow::{anyhow, Result};
use collections::BTreeMap;
use editor::{Editor, EditorElement, EditorStyle};
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gpui::{
    AnyView, AppContext, AsyncAppContext, FontStyle, ModelContext, Subscription, Task, TextStyle,
    View, WhiteSpace,
};
use http_client::HttpClient;
use kimi_ai::{
    stream_completion, KimiFunctionDefinition, KimiResponseStreamEvent, KimiToolChoice,
    KimiToolDefinition
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};
use std::{sync::Arc, time::Duration};
use strum::IntoEnumIterator;
use theme::ThemeSettings;
use ui::{prelude::*, Icon, IconName, Tooltip};
use util::ResultExt;

use crate::{
    settings::AllLanguageModelSettings, LanguageModel, LanguageModelId, LanguageModelName,
    LanguageModelProvider, LanguageModelProviderId, LanguageModelProviderName,
    LanguageModelProviderState, LanguageModelRequest, RateLimiter, Role,
};

const PROVIDER_ID: &str = "kimiai";
const PROVIDER_NAME: &str = "KimiAi";

#[derive(Default, Clone, Debug, PartialEq)]
pub struct KimiAiSettings {
    pub api_url: String,
    pub low_speed_timeout: Option<Duration>,
    pub available_models: Vec<KimiAvailableModel>,
    pub needs_setting_migration: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct KimiAvailableModel {
    pub name: String,
    pub max_tokens: usize,
    pub max_output_tokens: Option<u32>,
}

pub struct KimiAiLanguageModelProvider {
    http_client: Arc<dyn HttpClient>,
    state: gpui::Model<State>,
}

pub struct State {
    api_key: Option<String>,
    api_key_from_env: bool,
    _subscription: Subscription,
}

const KIMIAi_API_KEY_VAR: &'static str = "KIMIAI_API_KEY_VAR";

impl State {
    fn is_authenticated(&self) -> bool {
        self.api_key.is_some()
    }

    fn reset_api_key(&self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        let settings = &AllLanguageModelSettings::get_global(cx).kimiai;
        let delete_credentials = cx.delete_credentials(&settings.api_url);
        cx.spawn(|this, mut cx| async move {
            delete_credentials.await.log_err();
            this.update(&mut cx, |this, cx| {
                this.api_key = None;
                this.api_key_from_env = false;
                cx.notify();
            })
        })
    }

    fn set_api_key(&mut self, api_key: String, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        let settings = &AllLanguageModelSettings::get_global(cx).kimiai;
        let write_credentials =
            cx.write_credentials(&settings.api_url, "Bearer", api_key.as_bytes());

        cx.spawn(|this, mut cx| async move {
            write_credentials.await?;
            this.update(&mut cx, |this, cx| {
                this.api_key = Some(api_key);
                cx.notify();
            })
        })
    }

    fn authenticate(&self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        if self.is_authenticated() {
            Task::ready(Ok(()))
        } else {
            let api_url = AllLanguageModelSettings::get_global(cx)
                .kimiai
                .api_url
                .clone();
            cx.spawn(|this, mut cx| async move {
                let (api_key, from_env) = if let Ok(api_key) = std::env::var(KIMIAi_API_KEY_VAR) {
                    (api_key, true)
                } else {
                    let (_, api_key) = cx
                        .update(|cx| cx.read_credentials(&api_url))?
                        .await?
                        .ok_or_else(|| anyhow!("credentials not found"))?;
                    (String::from_utf8(api_key)?, false)
                };
                this.update(&mut cx, |this, cx| {
                    this.api_key = Some(api_key);
                    this.api_key_from_env = from_env;
                    cx.notify();
                })
            })
        }
    }
}

impl KimiAiLanguageModelProvider {
    pub fn new(http_client: Arc<dyn HttpClient>, cx: &mut AppContext) -> Self {
        let state = cx.new_model(|cx| State {
            api_key: None,
            api_key_from_env: false,
            _subscription: cx.observe_global::<SettingsStore>(|_this: &mut State, cx| {
                cx.notify();
            }),
        });

        Self { http_client, state }
    }
}

impl LanguageModelProviderState for KimiAiLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<gpui::Model<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for KimiAiLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        LanguageModelProviderId(PROVIDER_ID.into())
    }

    fn name(&self) -> LanguageModelProviderName {
        LanguageModelProviderName(PROVIDER_NAME.into())
    }

    fn icon(&self) -> IconName {
        IconName::AiKimi
    }

    fn provided_models(&self, cx: &AppContext) -> Vec<Arc<dyn LanguageModel>> {
        let mut models = BTreeMap::default();

        // Add base models from kimi_ai::Model::iter()
        for model in kimi_ai::Model::iter() {
            if !matches!(model, kimi_ai::Model::Custom { .. }) {
                models.insert(model.id().to_string(), model);
            }
        }

        // Override with available models from settings
        for model in &AllLanguageModelSettings::get_global(cx)
            .kimiai
            .available_models
        {
            models.insert(
                model.name.clone(),
                kimi_ai::Model::Custom {
                    name: model.name.clone(),
                    max_tokens: model.max_tokens,
                    max_output_tokens: model.max_output_tokens,
                },
            );
        }

        models
            .into_values()
            .map(|model| {
                Arc::new(KimiAiLanguageModel {
                    id: LanguageModelId::from(model.id().to_string()),
                    model,
                    state: self.state.clone(),
                    http_client: self.http_client.clone(),
                    request_limiter: RateLimiter::new(4),
                }) as Arc<dyn LanguageModel>
            })
            .collect()
    }

    fn is_authenticated(&self, cx: &AppContext) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut AppContext) -> Task<Result<()>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn configuration_view(&self, cx: &mut WindowContext) -> AnyView {
        cx.new_view(|cx| ConfigurationView::new(self.state.clone(), cx))
            .into()
    }

    fn reset_credentials(&self, cx: &mut AppContext) -> Task<Result<()>> {
        self.state.update(cx, |state, cx| state.reset_api_key(cx))
    }
}

pub struct KimiAiLanguageModel {
    id: LanguageModelId,
    model: kimi_ai::Model,
    state: gpui::Model<State>,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
}

impl KimiAiLanguageModel {
    fn stream_completion(
        &self,
        request: kimi_ai::Request,
        cx: &AsyncAppContext,
    ) -> BoxFuture<
        'static,
        Result<futures::stream::BoxStream<'static, Result<KimiResponseStreamEvent>>>,
    > {
        let http_client = self.http_client.clone();
        let Ok((api_key, api_url, low_speed_timeout)) = cx.read_model(&self.state, |state, cx| {
            let settings = &AllLanguageModelSettings::get_global(cx).kimiai;
            (
                state.api_key.clone(),
                settings.api_url.clone(),
                settings.low_speed_timeout,
            )
        }) else {
            return futures::future::ready(Err(anyhow!("App state dropped"))).boxed();
        };

        let future = self.request_limiter.stream(async move {
            let api_key = api_key.ok_or_else(|| anyhow!("missing api key"))?;
            let request = stream_completion(
                http_client.as_ref(),
                &api_url,
                &api_key,
                request,
                low_speed_timeout,
            );
            let response = request.await?;
            Ok(response)
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }
}

impl LanguageModel for KimiAiLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model.display_name().to_string())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        LanguageModelProviderId(PROVIDER_ID.into())
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        LanguageModelProviderName(PROVIDER_NAME.into())
    }

    fn telemetry_id(&self) -> String {
        format!("kimiai/{}", self.model.id())
    }

    fn max_token_count(&self) -> usize {
        self.model.max_token_count()
    }

    fn max_output_tokens(&self) -> Option<u32> {
        self.model.max_output_tokens()
    }

    fn count_tokens(
        &self,
        request: LanguageModelRequest,
        _cx: &AppContext,
    ) -> BoxFuture<'static, Result<usize>> {
        // count_kimi_ai_tokens(request, self.model.clone(), cx)
        let token_count = request
            .messages
            .iter()
            .map(|msg| msg.string_contents().chars().count())
            .sum::<usize>()
            *2;
        async move { Ok(token_count)}.boxed()
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncAppContext,
    ) -> BoxFuture<'static, Result<futures::stream::BoxStream<'static, Result<String>>>> {
        let request = request.into_kimi_ai(self.model.id().into(), self.max_output_tokens());
        let completions = self.stream_completion(request, cx);
        async move { Ok(kimi_ai::extract_text_from_events(completions.await?).boxed()) }.boxed()
    }

    fn use_any_tool(
        &self,
        request: LanguageModelRequest,
        tool_name: String,
        tool_description: String,
        schema: serde_json::Value,
        cx: &AsyncAppContext,
    ) -> BoxFuture<'static, Result<futures::stream::BoxStream<'static, Result<String>>>> {
        let mut request = request.into_kimi_ai(self.model.id().into(), self.max_output_tokens());
        request.tool_choice = Some(KimiToolChoice::Other(KimiToolDefinition::Function {
            function: KimiFunctionDefinition {
                name: tool_name.clone(),
                description: None,
                parameters: None,
            },
        }));
        request.tools = vec![KimiToolDefinition::Function {
            function: KimiFunctionDefinition {
                name: tool_name.clone(),
                description: Some(tool_description),
                parameters: Some(schema),
            },
        }];

        let response = self.stream_completion(request, cx);
        self.request_limiter
            .run(async move {
                let response = response.await?;
                Ok(
                    kimi_ai::extract_tool_args_from_events(tool_name, Box::pin(response))
                        .await?
                        .boxed(),
                )
            })
            .boxed()
    }
}

pub fn count_kimi_ai_tokens(
    request: LanguageModelRequest,
    model: kimi_ai::Model,
    cx: &AppContext,
) -> BoxFuture<'static, Result<usize>> {
    cx.background_executor()
        .spawn(async move {
            let messages = request
                .messages
                .into_iter()
                .map(|message| tiktoken_rs::ChatCompletionRequestMessage {
                    role: match message.role {
                        Role::User => "user".into(),
                        Role::Assistant => "assistant".into(),
                        Role::System => "system".into(),
                    },
                    content: Some(message.string_contents()),
                    name: None,
                    function_call: None,
                })
                .collect::<Vec<_>>();

            if let kimi_ai::Model::Custom { .. } = model {
                tiktoken_rs::num_tokens_from_messages("gpt-4", &messages)
            } else {
                tiktoken_rs::num_tokens_from_messages("gpt-4", &messages)
            }
        })
        .boxed()
}

struct ConfigurationView {
    api_key_editor: View<Editor>,
    state: gpui::Model<State>,
    load_credentials_task: Option<Task<()>>,
}

impl ConfigurationView {
    fn new(state: gpui::Model<State>, cx: &mut ViewContext<Self>) -> Self {
        let api_key_editor = cx.new_view(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_placeholder_text("sk-000000000000000000000000000000000000000000000000", cx);
            editor
        });

        cx.observe(&state, |_, _, cx| {
            cx.notify();
        })
        .detach();

        let load_credentials_task = Some(cx.spawn({
            let state = state.clone();
            |this, mut cx| async move {
                if let Some(task) = state
                    .update(&mut cx, |state, cx| state.authenticate(cx))
                    .log_err()
                {
                    // We don't log an error, because "not signed in" is also an error.
                    let _ = task.await;
                }

                this.update(&mut cx, |this, cx| {
                    this.load_credentials_task = None;
                    cx.notify();
                })
                .log_err();
            }
        }));

        Self {
            api_key_editor,
            state,
            load_credentials_task,
        }
    }

    fn save_api_key(&mut self, _: &menu::Confirm, cx: &mut ViewContext<Self>) {
        let api_key = self.api_key_editor.read(cx).text(cx);
        if api_key.is_empty() {
            return;
        }

        let state = self.state.clone();
        cx.spawn(|_, mut cx| async move {
            state
                .update(&mut cx, |state, cx| state.set_api_key(api_key, cx))?
                .await
        })
        .detach_and_log_err(cx);

        cx.notify();
    }

    fn reset_api_key(&mut self, cx: &mut ViewContext<Self>) {
        self.api_key_editor
            .update(cx, |editor, cx| editor.set_text("", cx));

        let state = self.state.clone();
        cx.spawn(|_, mut cx| async move {
            state
                .update(&mut cx, |state, cx| state.reset_api_key(cx))?
                .await
        })
        .detach_and_log_err(cx);

        cx.notify();
    }

    fn render_api_key_editor(&self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            font_style: FontStyle::Normal,
            line_height: relative(1.3),
            background_color: None,
            underline: None,
            strikethrough: None,
            white_space: WhiteSpace::Normal,
            truncate: None,
        };
        EditorElement::new(
            &self.api_key_editor,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }

    fn should_render_editor(&self, cx: &mut ViewContext<Self>) -> bool {
        !self.state.read(cx).is_authenticated()
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        const INSTRUCTIONS: [&str; 5] = [
            "To use the assistant panel or inline assistant, you need to add your KimiAi API key.",
            " - You can create an API key at: https://platform.moonshot.cn/console/api-keys",
            " - Make sure your KimiAi account has credits",
            "",
            "Paste your KimiAi API key below and hit enter to use the assistant:",
        ];

        let env_var_set = self.state.read(cx).api_key_from_env;

        if self.load_credentials_task.is_some() {
            div().child(Label::new("Loading credentials...")).into_any()
        } else if self.should_render_editor(cx) {
            v_flex()
                .size_full()
                .on_action(cx.listener(Self::save_api_key))
                .children(
                    INSTRUCTIONS.map(|instruction| Label::new(instruction)),
                )
                .child(
                    h_flex()
                        .w_full()
                        .my_2()
                        .px_2()
                        .py_1()
                        .bg(cx.theme().colors().editor_background)
                        .rounded_md()
                        .child(self.render_api_key_editor(cx)),
                )
                .child(
                    Label::new(
                        format!("You can also assign the {KIMIAi_API_KEY_VAR} environment variable and restart Zed."),
                    )
                    .size(LabelSize::Small),
                )
                .into_any()
        } else {
            h_flex()
                .size_full()
                .justify_between()
                .child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(IconName::Check).color(Color::Success))
                        .child(Label::new(if env_var_set {
                            format!("API key set in {KIMIAi_API_KEY_VAR} environment variable.")
                        } else {
                            "API key configured.".to_string()
                        })),
                )
                .child(
                    Button::new("reset-key", "Reset key")
                        .icon(Some(IconName::Trash))
                        .icon_size(IconSize::Small)
                        .icon_position(IconPosition::Start)
                        .disabled(env_var_set)
                        .when(env_var_set, |this| {
                            this.tooltip(|cx| Tooltip::text(format!("To reset your API key, unset the {KIMIAi_API_KEY_VAR} environment variable."), cx))
                        })
                        .on_click(cx.listener(|this, _, cx| this.reset_api_key(cx))),
                )
                .into_any()
        }
    }
}
