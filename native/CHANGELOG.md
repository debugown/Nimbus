## Nimbus 0.2.2

### Connection
- When Nimbus can't reach Discord, a line at the top of the window now says so and offers to fix it.
- The connection check shows step by step where the way to Discord breaks: the internet, Discord's address, the server, the website and the live connection.
- Repair tries a direct connection, then looks for a working proxy on this PC, and keeps the one that works.
- You can type your own HTTP or SOCKS5 proxy. Voice and streams don't go through a proxy: they need a VPN.
- Copy report gives a short text to send when asking for help. It has nothing about your account.
- Nimbus can also send that report to the developers, but only after you say yes, and only the text you were shown. The server sees your IP address, as any website does.
- If Discord doesn't answer when Nimbus starts, it keeps trying instead of stopping.

## Nimbus 0.2.1

### Servers
- Server folders: drag a server onto another one to make a folder, and drag to reorder. The order stays on this computer.
- The server menu can invite people, with a window listing your friends, and leave the server after a question.
- Servers you join or leave in another client appear on or leave the rail right away.
- Create Thread, Edit and Delete in the message menu work again.

### Messages
- New messages can come in with an animation of your choice, including one that types them out. See Settings, Appearance.
- Unread direct messages show under the DM button, as in Discord.
- Member lists load faster and no longer stay empty in some channels.
- GIFs are kept on disk, so they don't load again every time.

### Voice and streams
- Hover over the connection icon to see your ping, and click it for a connection window with a chart.
- Streams you watch should look sharper: Nimbus now tells Discord the size it wants, and big pictures are scaled down smoothly in small tiles.
- Your own stream shows up by itself when you start it.
- Better picture quality for streams sent from Nimbus, and the bitrate follows what Discord's server allows.
- A short drop of the connection to Discord no longer throws you out of voice and the stream. Nimbus picks up the same session again.
- No more false "Voice isn't working" message when only the stream has trouble.
- Smoother sound from music bots.

### Other
- The window can be moved by its top bar again.
- Part of this update was contributed by a member of the community. Thank you!

## Nimbus 0.2.0

### Languages
- Nimbus is now available in English, Russian, Ukrainian, Spanish, German, French, Portuguese (Brazil), Polish and Turkish.
- A new language picker with flags, in settings and on the sign-in screen.
- Translations were not reviewed by native speakers yet. If something reads wrong, please tell us.

### Navigation
- Opening a server brings you back to the chat you left there.
- The DM button opens your last direct message, or the friends page if there is none.
- Side panels can be resized by dragging their edges, and the size is remembered.

### Groups and member lists
- Group chats show who was added or removed, and calls, with icons and a Join the call link.
- Member list headings show the real number of people.
- Long search hints no longer run over the member list.

### Streams
- Sound from every stream you watch now plays together. Each stream has its own sound button.
- The call opens as a grid. One click makes a stream big, another click returns to the grid.

### Other
- Plus buttons and avatar letters are properly centred.
- Nimbus no longer writes a log file unless you ask it to (set the `NIMBUS_LOG` environment variable).
- After an update Nimbus now says so and shows what changed, right in the app. It is also in Settings, under About.

## Nimbus 0.1.9

### Streams
- Smoother streaming from Nimbus: automatic keyframes are now much rarer and large frames are sent faster. This targets the short, regular freezes viewers could see.
- New timing details in the log, to help track down any freezes that remain.


## Nimbus 0.1.8


### People and profiles
- Full right-click menu for people, on servers, in member lists, in voice and in the friends list: profile, mention, message, start call, note, friend nickname, invite to server, ignore, block and more.
- Moderator actions follow your real permissions and role position: nickname, roles, move, server mute and deafen, disconnect, kick, ban.
- Moderator view with member details and timeouts.
- New full profile window with tabs for activity, mutual friends and mutual servers, plus connections, notes and action buttons.
- All badges are shown, with a new badges window and progress for Nitro and server boost badges.
- App commands that act on a person are available from the menu.
- Voice verification codes can be viewed in encrypted calls.
- Open a person's Spotify track, or play it on your own Spotify with Listen along (needs Spotify Premium).

### Direct messages and groups
- Right-click menu in the DM list: mark as read, pin, favorite, mute for a chosen time, close the DM or leave the group, copy IDs. Pins and favorites are kept on this PC.
- Group chats now show their member list.
- Group system messages name the person who was added or removed.

### Voice and streams
- Watch several streams at once, switch between them, or see them in a grid.
- Picture-in-picture can be dragged anywhere in the window and has back, full screen and stop buttons.
- Fixed the people list in picture-in-picture showing only you.
- Streams should no longer freeze while the Nimbus window is being moved or resized.
- Faster start for other people's streams, and smoother sending of your own.
- Per-person controls: mute soundboard and turn off video, just for you.

### Speed
- Switching between chats and servers is much faster, using pages kept in memory.
- Recent chats are loaded ahead of time while the app starts.

### Other
- A green button appears in the title bar when an update is ready, and updates are checked every hour.
- The launcher window can be dragged.
- When Discord asks for verification on joining a server, a button opens the invite in official Discord.

