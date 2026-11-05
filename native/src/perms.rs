//! Which server channels we may see, from roles and channel overwrites
//! (Discord's own permission rules, as its client applies them).

use std::sync::Arc;

use serde_json::Value;

const CREATE_INVITE: u64 = 1 << 0;
const KICK_MEMBERS: u64 = 1 << 1;
const BAN_MEMBERS: u64 = 1 << 2;
const ADMINISTRATOR: u64 = 1 << 3;
const VIEW_CHANNEL: u64 = 1 << 10;
const MUTE_MEMBERS: u64 = 1 << 22;
const DEAFEN_MEMBERS: u64 = 1 << 23;
const MOVE_MEMBERS: u64 = 1 << 24;
const CHANGE_NICKNAME: u64 = 1 << 26;
const MANAGE_NICKNAMES: u64 = 1 << 27;
const MANAGE_ROLES: u64 = 1 << 28;
const MANAGE_EVENTS: u64 = 1 << 33;
const MODERATE_MEMBERS: u64 = 1 << 40;
const CREATE_EVENTS: u64 = 1 << 44;

/// What we may do to other members of a server (server-wide, as Discord's menu decides).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rights {
    pub mute: bool,
    pub deafen: bool,
    /// Move to another voice channel, or disconnect.
    pub move_members: bool,
    pub kick: bool,
    pub ban: bool,
    /// Change someone else's nickname.
    pub nick: bool,
    /// Change our own.
    pub own_nick: bool,
    pub manage_roles: bool,
    /// Time out: they can't write or talk for a while.
    pub timeout: bool,
}

/// What we need to know about ourselves in one server.
#[derive(Clone)]
pub struct Me {
    pub guild: String,
    pub user: String,
    pub owner: bool,
    pub roles: Vec<String>,
    /// Permissions from @everyone and our roles, before any channel overwrites.
    pub base: u64,
    /// All the server's roles, as Discord lists them.
    pub all_roles: Arc<Vec<Value>>,
}

fn position(role: &Value) -> i64 {
    role["position"].as_i64().unwrap_or(0)
}

fn bits(v: &Value) -> u64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0),
        Value::Number(n) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

impl Me {
    /// `roles` is the server's role list; `mine` our member's role ids.
    pub fn new(guild: &str, user: &str, owner: bool, roles: &[Value], mine: Vec<String>) -> Me {
        let base = roles
            .iter()
            .filter(|r| r["id"].as_str().is_some_and(|id| id == guild || mine.iter().any(|m| m == id)))
            .fold(0, |acc, r| acc | bits(&r["permissions"]));
        Me {
            guild: guild.into(),
            user: user.into(),
            owner,
            roles: mine,
            base,
            all_roles: Arc::new(roles.to_vec()),
        }
    }

    /// The highest of these roles, as Discord ranks them.
    fn top(&self, roles: &[String]) -> i64 {
        self.all_roles.iter().filter(|r| r["id"].as_str().is_some_and(|id| roles.iter().any(|m| m == id))).map(position).max().unwrap_or(0)
    }

    /// Our rights over one person: what touches their account needs a role above all of theirs,
    /// and nobody can do that to the server's owner. Voice actions don't look at roles.
    pub fn rights_over(&self, their_owner: bool, theirs: &[String]) -> Rights {
        let all = self.rights();
        let above = self.owner || (!their_owner && self.top(&self.roles) > self.top(theirs));
        Rights { kick: all.kick && above, ban: all.ban && above, nick: all.nick && above, manage_roles: all.manage_roles && above, timeout: all.timeout && above, ..all }
    }

    /// Whether we may give or take this role: below our highest one, and not one a bot or a boost holds.
    pub fn role_editable(&self, role: &Value) -> bool {
        role["managed"].as_bool() != Some(true) && self.rights().manage_roles && (self.owner || position(role) < self.top(&self.roles))
    }

    pub fn rights(&self) -> Rights {
        let all = self.owner || self.base & ADMINISTRATOR != 0;
        let has = |bit: u64| all || self.base & bit != 0;
        Rights {
            mute: has(MUTE_MEMBERS),
            deafen: has(DEAFEN_MEMBERS),
            move_members: has(MOVE_MEMBERS),
            kick: has(KICK_MEMBERS),
            ban: has(BAN_MEMBERS),
            nick: has(MANAGE_NICKNAMES),
            own_nick: has(CHANGE_NICKNAME) || has(MANAGE_NICKNAMES),
            manage_roles: has(MANAGE_ROLES),
            timeout: has(MODERATE_MEMBERS),
        }
    }

    /// Whether we may create scheduled events in this server.
    pub fn can_create_events(&self) -> bool {
        self.owner || self.base & (ADMINISTRATOR | MANAGE_EVENTS | CREATE_EVENTS) != 0
    }

    /// Whether the channel shows up for us.
    pub fn can_view(&self, channel: &Value) -> bool {
        self.owner || self.base & ADMINISTRATOR != 0 || self.in_channel(channel) & VIEW_CHANNEL != 0
    }

    /// Whether we may make an invite to the channel.
    pub fn can_invite(&self, channel: &Value) -> bool {
        const NEEDED: u64 = VIEW_CHANNEL | CREATE_INVITE;
        self.owner || self.base & ADMINISTRATOR != 0 || self.in_channel(channel) & NEEDED == NEEDED
    }

    /// Our permissions in a channel: the server's, then its overwrites in Discord's order.
    fn in_channel(&self, channel: &Value) -> u64 {
        let overwrites = channel["permission_overwrites"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let find = |id: &str| overwrites.iter().find(|o| o["id"].as_str() == Some(id));
        let apply = |p: u64, o: &Value| (p & !bits(&o["deny"])) | bits(&o["allow"]);

        let mut p = self.base;
        if let Some(o) = find(&self.guild) {
            p = apply(p, o);
        }
        let (mut allow, mut deny) = (0, 0);
        for o in self.roles.iter().filter_map(|r| find(r)) {
            allow |= bits(&o["allow"]);
            deny |= bits(&o["deny"]);
        }
        p = (p & !deny) | allow;
        if let Some(o) = find(&self.user) {
            p = apply(p, o);
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn me(base_view: bool, roles: Vec<&str>) -> Me {
        let everyone = if base_view { "1024" } else { "0" };
        let all = [
            json!({"id": "g", "permissions": everyone}),
            json!({"id": "mod", "permissions": "0"}),
        ];
        Me::new("g", "u", false, &all, roles.into_iter().map(String::from).collect())
    }

    #[test]
    fn an_invite_needs_its_own_permission() {
        let all = [json!({"id": "g", "permissions": "1025"})];
        let me = Me::new("g", "u", false, &all, vec![]);
        assert!(me.can_invite(&json!({})));
        let closed = json!({"permission_overwrites": [{"id": "g", "type": 0, "allow": "0", "deny": "1"}]});
        assert!(me.can_view(&closed));
        assert!(!me.can_invite(&closed));
    }

    #[test]
    fn everyone_denied_hides() {
        let c = json!({"permission_overwrites": [{"id": "g", "type": 0, "allow": "0", "deny": "1024"}]});
        assert!(!me(true, vec![]).can_view(&c));
        assert!(me(true, vec![]).can_view(&json!({})));
    }

    #[test]
    fn role_allow_beats_everyone_deny() {
        let c = json!({"permission_overwrites": [
            {"id": "g", "type": 0, "allow": "0", "deny": "1024"},
            {"id": "mod", "type": 0, "allow": "1024", "deny": "0"}
        ]});
        assert!(me(true, vec!["mod"]).can_view(&c));
        assert!(!me(true, vec![]).can_view(&c));
    }

    #[test]
    fn rights_from_roles() {
        let roles = [
            json!({"id": "g", "permissions": "0"}),
            json!({"id": "mod", "permissions": ((1u64 << 22) | (1 << 1)).to_string()}),
        ];
        let plain = Me::new("g", "u", false, &roles, vec![]).rights();
        assert_eq!(plain, Rights::default());
        let moderator = Me::new("g", "u", false, &roles, vec!["mod".into()]).rights();
        assert!(moderator.mute && moderator.kick && !moderator.ban && !moderator.move_members);
        let owner = Me::new("g", "u", true, &roles, vec![]).rights();
        assert!(owner.mute && owner.deafen && owner.move_members && owner.kick && owner.ban);
    }

    #[test]
    fn a_higher_role_protects_its_holder() {
        let roles = [
            json!({"id": "g", "position": 0, "permissions": "0"}),
            json!({"id": "plain", "position": 1, "permissions": "0"}),
            json!({"id": "mod", "position": 2, "permissions": (KICK_MEMBERS | BAN_MEMBERS | MUTE_MEMBERS | MANAGE_NICKNAMES | MANAGE_ROLES).to_string()}),
            json!({"id": "boss", "position": 3, "permissions": "0"}),
        ];
        let me = Me::new("g", "u", false, &roles, vec!["mod".into()]);
        let below = me.rights_over(false, &["plain".to_string()]);
        assert!(below.kick && below.ban && below.nick && below.manage_roles);
        for theirs in [vec!["boss".to_string()], vec!["mod".to_string()]] {
            let over = me.rights_over(false, &theirs);
            assert!(!over.kick && !over.ban && !over.nick && !over.manage_roles, "{theirs:?}");
            assert!(over.mute, "voice actions don't look at roles");
        }
        assert!(!me.rights_over(true, &[]).kick, "nobody kicks the owner");
        assert!(Me::new("g", "u", true, &roles, vec![]).rights_over(false, &["boss".to_string()]).kick, "the owner may");
    }

    #[test]
    fn only_roles_below_ours_can_be_given() {
        let boss = json!({"id": "boss", "position": 3, "permissions": "0"});
        let plain = json!({"id": "plain", "position": 1, "permissions": "0"});
        let bot = json!({"id": "bot", "position": 1, "permissions": "0", "managed": true});
        let moderator = json!({"id": "mod", "position": 2, "permissions": MANAGE_ROLES.to_string()});
        let roles = [boss.clone(), plain.clone(), bot.clone(), moderator.clone()];
        let me = Me::new("g", "u", false, &roles, vec!["mod".into()]);
        assert!(me.role_editable(&plain));
        assert!(!me.role_editable(&boss) && !me.role_editable(&moderator) && !me.role_editable(&bot));
        let nobody = Me::new("g", "u", false, &roles, vec![]);
        assert!(!nobody.role_editable(&plain));
        let owner = Me::new("g", "u", true, &roles, vec![]);
        assert!(owner.role_editable(&boss) && !owner.role_editable(&bot));
    }

    #[test]
    fn event_rights() {
        let roles = |bits: u64| [json!({"id": "g", "permissions": bits.to_string()})];
        assert!(!Me::new("g", "u", false, &roles(0), vec![]).can_create_events());
        assert!(Me::new("g", "u", false, &roles(1 << 33), vec![]).can_create_events());
        assert!(Me::new("g", "u", false, &roles(1 << 44), vec![]).can_create_events());
        assert!(Me::new("g", "u", true, &roles(0), vec![]).can_create_events());
    }

    #[test]
    fn member_deny_and_admin() {
        let c = json!({"permission_overwrites": [{"id": "u", "type": 1, "allow": "0", "deny": "1024"}]});
        assert!(!me(true, vec![]).can_view(&c));
        let admin = Me::new("g", "u", false, &[json!({"id": "g", "permissions": "8"})], vec![]);
        assert!(admin.can_view(&c));
    }
}
