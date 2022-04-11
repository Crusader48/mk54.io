// SPDX-FileCopyrightText: 2022 Softbear, Inc.

use crate::translation::t;
use core_protocol::name::PlayerAlias;
use stylist::yew::styled_component;
use web_sys::HtmlInputElement;
use yew::prelude::*;

#[derive(Default, PartialEq, Properties)]
pub struct DialogProps {
    pub on_play: Callback<PlayerAlias>,
    pub children: Children,
}

#[styled_component(SpawnOverlay)]
pub fn spawn_overlay(props: &DialogProps) -> Html {
    let div_style = css!(
        r#"
        display: flex;
        flex-direction: column;
        font-size: 2rem;
        left: 50%;
        position: absolute;
        row-gap: 2rem;
        top: 50%;
        transform: translate(-50%, -50%);
        user-select: none;
        width: 50%;
    "#
    );

    let button_style = css!(
        r#"
        background-color: #549f57;
        border-radius: 1rem;
        border: 1px solid #61b365;
        box-sizing: border-box;
        color: white;
        cursor: pointer;
        font-size: 4rem;
        left: 50%;
        margin-top: 0.5em;
        min-width: 15rem;
        padding-bottom: 0.7rem;
        padding-top: 0.5rem;
        position: relative;
        text-decoration: none;
        transform: translate(-50%, 0%);
        white-space: nowrap;
        width: min-content;

        :disabled {
            filter: opacity(0.6);
        }

        :hover:not(:disabled) {
            filter: brightness(0.95);
        }

        :active:not(:disabled) {
            filter: brightness(0.9);
        }
    "#
    );

    let input_style = css!(
        r#"
        background-color: #00000025;
        border-radius: 3rem;
        border: 0;
        box-sizing: border-box;
        color: #FFFA;
        cursor: pointer;
        font-size: 1em;
        font-weight: bold;
        margin-top: 0.25em;
        outline: 0;
        padding-left: 2rem;
        padding: 0.7em;
        pointer-events: all;
        text-align: center;
        white-space: nowrap;
        width: 100%;
   "#
    );

    let paused = false; // TODO
    let transitioning = false; // TODO

    let alias = use_state(|| PlayerAlias::new_unsanitized(""));

    let oninput = {
        let alias = alias.clone();
        Callback::from(move |event: InputEvent| {
            alias.set(PlayerAlias::new_unsanitized(
                &event.target_unchecked_into::<HtmlInputElement>().value(),
            ))
        })
    };

    let onclick = {
        let alias = alias.clone();
        props.on_play.reform(move |_: MouseEvent| *alias)
    };

    let onsubmit = {
        let alias = alias.clone();
        // [`FocusEvent`] instead of [`SubmitEvent`] due to:
        // - https://github.com/rustwasm/wasm-bindgen/issues/2712
        // - https://github.com/yewstack/yew/issues/1359
        props.on_play.reform(move |event: FocusEvent| {
            event.prevent_default();
            *alias
        })
    };

    // <svelte:window on:message={handleMessage}/>
    // <div id="spawn_overlay" in:fade={transition} on:introstart={() => transitioning = true} on:introend={() => transitioning = false}>
    html! {
        <form id="spawn_overlay" class={div_style} {onsubmit}>
            {props.children.clone()}
            <input id="alias_input" class={input_style} disabled={paused || transitioning} type="text" name="name" placeholder={t().splash_screen_alias_placeholder()} autocomplete="off" value={alias.to_string()} {oninput}/>
            <button id="play_button" class={button_style} disabled={paused || transitioning} {onclick}>{t().splash_screen_play_button()}</button>
        </form>
    }
}
