//! 短信页与设置项（v0.6.51）。
//!
//! 页面形态跟邮箱页一致：左边一张验证码卡片，右边是最近收到的短信列表。
//! 设置里给出转发地址与 SmsForwarder 的 JSON 模板，抄过去就能用。

use crate::app::{App, Cmd, SMS_DETAIL_H, SMS_H};
use crate::extras::{setting_row, settings_card, settings_label};
use crate::paint::{capsule, Card, H};
use crate::settings_theme::color as settings_color;
use crate::sms;
use crate::theme::{self, size, W};
use eframe::egui::{self, Color32};

const ROW_H: f32 = 34.0;
const CARD_W: f32 = 140.0;
const CARD_H: f32 = 110.0;
const SETTINGS: usize = 12;

/// 设置页里预览长模板用；超出部分用省略号收掉。
fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn clock(at: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(time) = Local.timestamp_opt(at, 0).single() else { return String::new() };
    if time.date_naive() == Local::now().date_naive() {
        time.format("%H:%M").to_string()
    } else {
        time.format("%m-%d %H:%M").to_string()
    }
}

impl App {
    fn sms_address(&self) -> String {
        format!("http://{}:{}{}", self.sms.local.as_deref().unwrap_or("127.0.0.1"), self.settings.sms_port, sms::PATH)
    }

    /// 状态文案 + 颜色：错误优先，其次「在监听」，最后「没开」。
    fn sms_status_text(&self) -> (String, Color32) {
        if let Some(error) = &self.sms.error {
            return (error.clone(), theme::RED_TOP);
        }
        if !self.settings.sms_enabled {
            return ("短信转发未开启".into(), theme::DIM);
        }
        if self.sms.running {
            (format!("{} 正在监听 · 已收 {} 条", self.sms_address(), self.sms.recent.len()), theme::GREEN)
        } else {
            ("接收服务未启动".into(), theme::DIM)
        }
    }

    pub(crate) fn page_sms(&mut self, c: &Card, cmds: &mut Vec<Cmd>) {
        // 启动时没联网的话这里是空的，进页面时再取一次。
        if self.sms.local.is_none() {
            self.sms.local = crate::sms::local_ipv4();
        }
        if !self.settings.sms_enabled {
            c.text(42.0, 60.0, "短信转发未开启", 13.0, W::Bold, theme::WHITE, H::Left);
            let hint = c.fit("开启后手机把短信推到本机端口，验证码会自动上岛", 10.5, W::Regular, 344.0);
            c.text(42.0, 77.0, &hint, 10.5, W::Regular, theme::GRAY, H::Left);
            c.text(42.0, 94.0, "设置 → 短信 里有转发地址和模板", 10.0, W::Regular, theme::DIM, H::Left);
            let (go, gc) = capsule(c, 42.0, 104.0, 84.0, 26.0, "sms-setup", theme::BTN, theme::BTN_HOVER);
            gc.text(58.0, 121.0, "去设置", size::BODY, W::Regular, theme::WHITE, H::Left);
            if go.clicked() {
                self.settings_tab = SETTINGS;
                self.open_settings();
            }
            return;
        }

        // 全文态：点开一条没有验证码的短信后，整页换成正文（长短信可滚动）。
        if let Some(id) = self.sms.open_id {
            let Some(item) = self.sms.recent.iter().find(|item| item.id == id).cloned() else {
                self.sms.open_id = None;
                return;
            };
            let (back, bc) = capsule(c, 36.0, 34.0, 56.0, 24.0, "sms-back", theme::BTN, theme::BTN_HOVER);
            bc.text(50.0, 46.0, "返回", 10.5, W::Regular, theme::WHITE, H::Left);
            if back.clicked() {
                self.sms.open_id = None;
            }
            c.text(100.0, 46.0, &c.fit(&item.from, 11.5, W::Semibold, 176.0), 11.5, W::Semibold, theme::WHITE, H::Left);
            c.text(383.0, 46.0, &clock(item.at), 10.0, W::Regular, Color32::from_gray(170), H::Right);
            if let Some(code) = &item.code {
                c.text(100.0, 62.0, &format!("验证码 {code}"), 10.0, W::Semibold, theme::GREEN, H::Left);
            }
            let body = c.rect(36.0, 76.0, 347.0, SMS_DETAIL_H - 128.0);
            let mut area = egui::Ui::new(
                c.ui.ctx().clone(),
                egui::Id::new(("sms-body", c.live)),
                egui::UiBuilder::new().max_rect(body).layer_id(c.painter.layer_id()).style(c.ui.style().clone()),
            );
            area.set_clip_rect(body.intersect(c.painter.clip_rect()));
            area.set_opacity(c.painter.opacity());
            if !c.live {
                area.disable();
            }
            area.spacing_mut().scroll.bar_width = 3.0 * c.s;
            egui::ScrollArea::vertical()
                .id_salt("sms-body-scroll")
                .max_height(body.height())
                .auto_shrink([false, false])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                .show(&mut area, |ui| {
                    ui.label(egui::RichText::new(&item.text).size(12.0).color(Color32::from_gray(224)));
                });
            let (copy, cc) = capsule(c, 36.0, SMS_DETAIL_H - 44.0, 92.0, 24.0, "sms-copy-all", Color32::from_gray(32), Color32::from_gray(48));
            cc.text(52.0, SMS_DETAIL_H - 32.0, "复制全文", 10.5, W::Regular, theme::WHITE, H::Left);
            if copy.clicked() {
                cmds.push(Cmd::Copy(item.text.clone()));
                self.say("短信内容已复制");
            }
            return;
        }

        if self.sms.recent.is_empty() {
            let (status, color) = self.sms_status_text();
            c.text(42.0, 58.0, "等待手机转发", 13.0, W::Bold, theme::WHITE, H::Left);
            let hint = if self.sms.local.is_some() {
                c.fit(&format!("在手机转发 App 里填写 {}", self.sms_address()), 10.5, W::Regular, 344.0)
            } else {
                "这台电脑没有局域网地址，请先连上 Wi-Fi 或网线".into()
            };
            c.text(42.0, 75.0, &hint, 10.5, W::Regular, theme::GRAY, H::Left);
            c.text(42.0, 92.0, &c.fit(&status, 10.0, W::Regular, 344.0), 10.0, W::Regular, color, H::Left);
            let (go, gc) = capsule(c, 42.0, 102.0, 104.0, 26.0, "sms-config", theme::BTN, theme::BTN_HOVER);
            gc.text(56.0, 119.0, "转发设置", size::BODY, W::Regular, theme::WHITE, H::Left);
            if go.clicked() {
                self.settings_tab = SETTINGS;
                self.open_settings();
            }
            return;
        }

        // 最新一条带验证码的消息放在卡片里；没有验证码时列表占满整行。
        let latest = self.sms.recent.iter().find(|item| item.code.is_some()).cloned();
        let list_area = if latest.is_some() { c.rect(182.0, 36.0, 201.0, CARD_H) } else { c.rect(36.0, 36.0, 347.0, CARD_H) };
        let mut clicked: Option<(String, String)> = None;
        let mut opened: Option<u64> = None;

        if let Some(item) = &latest {
            let code = item.code.clone().unwrap_or_default();
            c.fill(36.0, 36.0, CARD_W, CARD_H, 11.0, theme::CARD);
            c.text(46.0, 51.0, "验证码", 10.5, W::Regular, theme::GRAY, H::Left);
            c.text(166.0, 51.0, &c.fit(&item.from, 10.5, W::Semibold, 84.0), 10.5, W::Semibold, theme::WHITE, H::Right);
            c.text(46.0, 81.0, &c.fit(&code, 23.0, W::Bold, 120.0), 23.0, W::Bold, theme::GREEN, H::Left);
            c.text(46.0, 98.0, "点击复制", 10.5, W::Regular, theme::DIM, H::Left);
            c.text(166.0, 98.0, &clock(item.at), 10.0, W::Regular, theme::GRAY, H::Right);
            let hint = format!("来自：{}\n{}\n{}", item.from, item.text, clock(item.at));
            c.hit(40.0, 38.0, CARD_W - 8.0, 40.0, ("sms-code-src", item.id)).on_hover_text(&hint);
            let copy = c.hit(40.0, 60.0, CARD_W - 8.0, 44.0, ("sms-code", item.id)).on_hover_text(&hint);
            if copy.clicked() {
                clicked = Some((code.clone(), code.clone()));
            }
            // 底部状态：让「服务在不在跑」随时可见。
            let (status, color) = self.sms_status_text();
            let status = c.fit(&status, 10.0, W::Regular, 344.0);
            c.text(36.0, SMS_H - 22.0, &status, 10.0, W::Regular, color, H::Left);
        }

        let row_width = list_area.width() / c.s - 4.0;
        let recent = &self.sms.recent;
        let mut content = egui::Ui::new(
            c.ui.ctx().clone(),
            egui::Id::new(("sms-list", c.live)),
            egui::UiBuilder::new().max_rect(list_area).layer_id(c.painter.layer_id()).style(c.ui.style().clone()),
        );
        content.set_clip_rect(list_area.intersect(c.painter.clip_rect()));
        content.set_opacity(c.painter.opacity());
        if !c.live {
            content.disable();
        }
        content.spacing_mut().item_spacing.y = 0.0;
        content.spacing_mut().scroll.floating = false;
        content.spacing_mut().scroll.bar_width = 3.0 * c.s;
        content.visuals_mut().widgets.inactive.bg_fill = Color32::from_gray(80);
        egui::ScrollArea::vertical()
            .id_salt("sms-list-scroll")
            .max_height(list_area.height())
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
            .show(&mut content, |ui| {
                for item in recent {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(row_width * c.s, ROW_H * c.s), egui::Sense::hover());
                    let mut row = Card::new(ui, rect.min, ui.clip_rect());
                    row.s = c.s;
                    row.live = c.live;
                    let hit = row.hit(0.0, 0.0, row_width, ROW_H - 1.0, ("sms-row", item.id));
                    if hit.hovered() {
                        row.fill(0.0, 0.0, row_width, ROW_H - 1.0, 6.0, Color32::from_gray(27));
                    }
                    let from = row.fit(&item.from, 11.5, W::Semibold, row_width - 56.0);
                    row.text(6.0, 13.0, &from, 11.5, W::Semibold, theme::WHITE, H::Left);
                    row.text(row_width - 4.0, 13.0, &clock(item.at), 10.0, W::Regular, Color32::from_gray(170), H::Right);
                    let mut x = 6.0;
                    if let Some(code) = &item.code {
                        let label = format!("验证码 {code}");
                        x += row.text(6.0, 28.0, &label, 10.5, W::Semibold, theme::GREEN, H::Left) + 6.0;
                    }
                    let rest = row_width - x - 4.0;
                    if rest > 12.0 {
                        row.text(x, 28.0, &row.fit(&item.text, 10.5, W::Regular, rest), 10.5, W::Regular, Color32::from_gray(190), H::Left);
                    }
                    let hint = format!("来自：{}\n{}\n{}", item.from, item.text, clock(item.at));
                    hit.clone().on_hover_text(&hint);
                    if hit.clicked() {
                        match &item.code {
                            // 有验证码：点一下直接复制，这是最高频的动作。
                            Some(code) => clicked = Some((code.clone(), code.clone())),
                            // 没验证码：进全文，别再自动复制——用户要的是"看清写了什么"。
                            None => opened = Some(item.id),
                        }
                    }
                }
            });

        if latest.is_none() {
            let (status, color) = self.sms_status_text();
            let status = c.fit(&status, 10.0, W::Regular, 344.0);
            c.text(36.0, SMS_H - 22.0, &status, 10.0, W::Regular, color, H::Left);
        }
        if let Some((text, code)) = clicked {
            cmds.push(Cmd::Copy(text));
            if code.is_empty() {
                self.say("短信内容已复制");
            } else {
                self.code_copied(&code);
            }
        }
        if let Some(id) = opened {
            self.sms.open_id = Some(id);
        }
    }

    pub(crate) fn sms_settings(&mut self, ui: &mut egui::Ui) {
        if self.sms.local.is_none() {
            self.sms.local = crate::sms::local_ipv4();
        }
        ui.label(egui::RichText::new("手机收到的短信转发到这台电脑，验证码会自动上岛。只在本机局域网监听，不经过任何服务器。").size(12.0).color(settings_color(ui.ctx(), Color32::from_gray(180), Color32::from_gray(75))));

        settings_label(ui, "接收服务");
        let was_enabled = self.settings.sms_enabled;
        settings_card(ui, |ui| {
            setting_row(ui, "短信转发", "开启后监听端口，收到的短信显示在「短信」页", &mut self.settings.sms_enabled);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("端口").size(13.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(egui::DragValue::new(&mut self.settings.sms_port).range(1024..=65535).speed(1.0));
                    ui.label(egui::RichText::new("1024 以上，改完自动重启监听").size(11.0).color(settings_color(ui.ctx(), Color32::from_gray(160), Color32::from_gray(90))));
                });
            });
            ui.separator();
            let (status, color) = self.sms_status_text();
            let color = if color == theme::GREEN { Color32::from_rgb(48, 184, 92) } else if color == theme::RED_TOP { Color32::from_rgb(215, 64, 64) } else { settings_color(ui.ctx(), Color32::from_gray(165), Color32::from_gray(95)) };
            ui.label(egui::RichText::new(status).size(11.5).color(color));
        });
        // 开启转发就把短信页带进轮播，省得用户再去「页面管理」翻一遍。
        if !was_enabled && self.settings.sms_enabled {
            self.set_page_visible(8, true);
        }

        settings_label(ui, "转发地址");
        settings_card(ui, |ui| {
            let address = self.sms_address();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&address).size(12.5).monospace().color(settings_color(ui.ctx(), Color32::WHITE, Color32::from_rgb(30, 34, 41))));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("复制").clicked() {
                        ui.ctx().copy_text(address.clone());
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("令牌 {}", self.settings.sms_token)).size(11.5).monospace().color(settings_color(ui.ctx(), Color32::from_gray(175), Color32::from_gray(85))));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("复制").clicked() {
                        ui.ctx().copy_text(self.settings.sms_token.clone());
                    }
                });
            });
            ui.label(egui::RichText::new("手机必须和电脑处在同一个 Wi-Fi 或局域网；换网络后地址可能变化。").size(11.0).color(settings_color(ui.ctx(), Color32::from_gray(150), Color32::from_gray(100))));
        });

        settings_label(ui, "手机端配置（SmsForwarder 为例）");
        settings_card(ui, |ui| {
            let template = format!(
                "{{\"token\":\"{}\",\"from\":\"{{{{FROM}}}}\",\"content\":\"{{{{SMS}}}}\",\"timestamp\":\"{{{{RECEIVE_TIME}}}}\"}}",
                self.settings.sms_token
            );
            for (index, step) in [
                "1. 发送通道 → 新建 → Webhook（或「自定义」），请求方式选 POST",
                "2. URL 填上面的转发地址，请求头不用加（令牌写在请求体里）",
                "3. 消息模板粘贴下面这段（3.x 用双花括号；2.x 老版本改成 [from] [sms] [timestamp]）",
                "4. 转发规则里把「启用自定义模板」关掉——它的优先级高于通道模板，拼出来不是 JSON",
                "5. 规则只勾「收到短信」，并关闭系统对它的电池优化，否则锁屏后会被杀",
            ]
            .iter()
            .enumerate()
            {
                ui.label(egui::RichText::new(*step).size(11.5).color(settings_color(ui.ctx(), Color32::from_gray(190), Color32::from_gray(60))));
                if index == 2 {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(clip(&template, 52)).size(11.0).monospace().color(settings_color(ui.ctx(), theme::SOFT_GREEN, Color32::from_rgb(20, 90, 50))));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("复制模板").clicked() {
                                ui.ctx().copy_text(template.clone());
                            }
                        });
                    });
                }
            }
            ui.separator();
            ui.label(egui::RichText::new("Windows 第一次监听端口时会弹防火墙提示，勾「专用网络」允许即可；别把端口映射到公网。").size(11.0).color(settings_color(ui.ctx(), Color32::from_gray(150), Color32::from_gray(100))));
        });

        // 内存里的短信在设置里也能翻：不用为了看一眼全文特意把岛展开。
        let count = self.sms.recent.len();
        settings_label(ui, &if count == 0 { "最近收到".to_string() } else { format!("最近收到 · {count} 条") });
        settings_card(ui, |ui| {
            if self.sms.recent.is_empty() {
                ui.label(egui::RichText::new("还没有收到短信。手机转发成功后会自动出现在这里，重启后清空（只留在内存）。").size(11.5).color(settings_color(ui.ctx(), Color32::from_gray(160), Color32::from_gray(95))));
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("sms-settings-history")
                .max_height(240.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for item in &self.sms.recent {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&item.from).size(12.0).strong().color(settings_color(ui.ctx(), Color32::from_gray(230), Color32::from_rgb(30, 34, 41))));
                            if let Some(code) = &item.code {
                                ui.label(egui::RichText::new(format!("验证码 {code}")).size(11.0).color(Color32::from_rgb(48, 184, 92)));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("复制全文").clicked() {
                                    ui.ctx().copy_text(item.text.clone());
                                }
                                ui.label(egui::RichText::new(clock(item.at)).size(10.5).color(settings_color(ui.ctx(), Color32::from_gray(145), Color32::from_gray(110))));
                            });
                        });
                        ui.label(egui::RichText::new(&item.text).size(11.5).color(settings_color(ui.ctx(), Color32::from_gray(185), Color32::from_gray(70))));
                        ui.separator();
                    }
                });
        });
    }
}
