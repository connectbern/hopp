//go:build integration
// +build integration

package integration

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"

	"hopp-backend/internal/models"
	"hopp-backend/internal/server"
)

type remoteCommandResult struct {
	Online    bool                   `json:"online"`
	Delivered bool                   `json:"delivered"`
	OK        bool                   `json:"ok"`
	Message   string                 `json:"message"`
	Status    map[string]interface{} `json:"status"`
}

// remoteControlFixture: owner + friend + bystander share a team, outsider is in
// another team.
type remoteControlFixture struct {
	srv       *server.Server
	owner     *models.User
	friend    *models.User
	bystander *models.User
	outsider  *models.User
}

func newRemoteControlFixture(t *testing.T) (*remoteControlFixture, func()) {
	t.Helper()
	srv, closeServer := setupTestServerWithRedis(t)
	// Registered before any websocket is opened, so it runs after the fake apps
	// have disconnected (t.Cleanup is last-in-first-out): Redis must outlive the
	// websocket handlers that are subscribed to it.
	t.Cleanup(func() {
		time.Sleep(150 * time.Millisecond)
		closeServer()
	})
	cleanup := func() {}

	team := createTestTeam(t, srv.DB, "Remote Team")
	other := createTestTeam(t, srv.DB, "Other Team")

	mk := func(email, first string, teamID uint) *models.User {
		u := &models.User{
			UserProfile: models.UserProfile{FirstName: first, LastName: "Test", Email: email, TeamID: &teamID},
			Password:    "a-long-enough-password",
		}
		require.NoError(t, srv.DB.Create(u).Error)
		return u
	}

	return &remoteControlFixture{
		srv:       srv,
		owner:     mk("owner@example.com", "Owner", team.ID),
		friend:    mk("friend@example.com", "Friend", team.ID),
		bystander: mk("bystander@example.com", "Bystander", team.ID),
		outsider:  mk("outsider@example.com", "Outsider", other.ID),
	}, cleanup
}

func (f *remoteControlFixture) do(t *testing.T, method, path, email string, payload interface{}) *httptest.ResponseRecorder {
	t.Helper()
	var body *bytes.Reader
	if payload != nil {
		raw, err := json.Marshal(payload)
		require.NoError(t, err)
		body = bytes.NewReader(raw)
	} else {
		body = bytes.NewReader(nil)
	}
	req := httptest.NewRequest(method, path, body)
	req.Header.Set("Content-Type", "application/json")
	if email != "" {
		req.Header.Set("Authorization", "Bearer "+getJWTToken(t, f.srv, email))
	}
	rec := httptest.NewRecorder()
	f.srv.Echo.ServeHTTP(rec, req)
	return rec
}

func (f *remoteControlFixture) allow(t *testing.T, enabled bool, allowed ...*models.User) {
	t.Helper()
	ids := make([]string, 0, len(allowed))
	for _, u := range allowed {
		ids = append(ids, u.ID)
	}
	rec := f.do(t, http.MethodPut, "/api/auth/remote-control/settings", f.owner.Email, map[string]interface{}{
		"enabled":          enabled,
		"allowed_user_ids": ids,
	})
	require.Equal(t, http.StatusOK, rec.Code, rec.Body.String())
}

func (f *remoteControlFixture) command(t *testing.T, email, targetID, command string) *httptest.ResponseRecorder {
	t.Helper()
	return f.do(t, http.MethodPost, "/api/auth/remote-control/"+targetID+"/command", email, map[string]interface{}{"command": command})
}

// fakeApp is a headless stand-in for the desktop app: it holds the user's
// websocket and answers remote commands.
type fakeApp struct {
	conn     *websocket.Conn
	commands chan map[string]interface{}
}

func connectFakeApp(t *testing.T, httpSrv *httptest.Server, srv *server.Server, email string, answer func(payload map[string]interface{}) map[string]interface{}) *fakeApp {
	t.Helper()
	url := "ws" + strings.TrimPrefix(httpSrv.URL, "http") + "/api/auth/websocket?token=" + getJWTToken(t, srv, email)
	conn, _, err := websocket.DefaultDialer.Dial(url, nil)
	require.NoError(t, err)

	app := &fakeApp{conn: conn, commands: make(chan map[string]interface{}, 16)}

	// First frame is the "success" greeting.
	_, _, err = conn.ReadMessage()
	require.NoError(t, err)

	go func() {
		for {
			_, raw, err := conn.ReadMessage()
			if err != nil {
				return
			}
			var msg struct {
				Type    string                 `json:"type"`
				Payload map[string]interface{} `json:"payload"`
			}
			if json.Unmarshal(raw, &msg) != nil || msg.Type != "remote_command" {
				continue
			}
			app.commands <- msg.Payload
			if answer == nil {
				continue
			}
			if result := answer(msg.Payload); result != nil {
				_ = conn.WriteJSON(map[string]interface{}{"type": "remote_command_result", "payload": result})
			}
		}
	}()

	t.Cleanup(func() { conn.Close() })
	return app
}

// waitOnline blocks until the server sees the user's websocket subscription.
func waitOnline(t *testing.T, f *remoteControlFixture, user *models.User) {
	t.Helper()
	require.Eventually(t, func() bool {
		rec := f.do(t, http.MethodGet, "/api/auth/teammates", f.bystander.Email, nil)
		var mates []struct {
			ID       string `json:"id"`
			IsActive bool   `json:"is_active"`
		}
		if json.Unmarshal(rec.Body.Bytes(), &mates) != nil {
			return false
		}
		for _, m := range mates {
			if m.ID == user.ID {
				return m.IsActive
			}
		}
		return false
	}, 3*time.Second, 20*time.Millisecond)
}

func TestRemoteControl_Unauthenticated(t *testing.T) {
	f, cleanup := newRemoteControlFixture(t)
	defer cleanup()

	for _, tc := range []struct{ method, path string }{
		{http.MethodGet, "/api/auth/remote-control/settings"},
		{http.MethodPut, "/api/auth/remote-control/settings"},
		{http.MethodGet, "/api/auth/remote-control/targets"},
		{http.MethodPost, "/api/auth/remote-control/" + f.owner.ID + "/command"},
	} {
		rec := f.do(t, tc.method, tc.path, "", map[string]interface{}{"command": "start_share", "enabled": true})
		assert.Contains(t, []int{http.StatusUnauthorized, http.StatusBadRequest}, rec.Code, "%s %s", tc.method, tc.path)
		assert.NotEqual(t, http.StatusOK, rec.Code, "%s %s", tc.method, tc.path)
	}

	// Nothing was switched on by the unauthenticated PUT.
	setting, err := models.GetRemoteControlSetting(f.srv.DB, f.owner.ID)
	require.NoError(t, err)
	assert.False(t, setting.Enabled)
}

func TestRemoteControl_OffByDefault(t *testing.T) {
	f, cleanup := newRemoteControlFixture(t)
	defer cleanup()

	// No settings row at all: every teammate is refused, nobody is listed.
	for _, cmd := range []string{"status", "start_share", "stop_share", "join", "leave", "restart"} {
		rec := f.command(t, f.friend.Email, f.owner.ID, cmd)
		assert.Equal(t, http.StatusForbidden, rec.Code, cmd)
	}
	// Not even the owner's own account while the feature is off.
	assert.Equal(t, http.StatusForbidden, f.command(t, f.owner.Email, f.owner.ID, "status").Code)

	rec := f.do(t, http.MethodGet, "/api/auth/remote-control/targets", f.friend.Email, nil)
	require.Equal(t, http.StatusOK, rec.Code)
	assert.JSONEq(t, `{"targets":[]}`, rec.Body.String())

	rec = f.do(t, http.MethodGet, "/api/auth/remote-control/settings", f.owner.Email, nil)
	require.Equal(t, http.StatusOK, rec.Code)
	assert.JSONEq(t, `{"enabled":false,"allowed_user_ids":[]}`, rec.Body.String())
}

func TestRemoteControl_AllowListEnforced(t *testing.T) {
	f, cleanup := newRemoteControlFixture(t)
	defer cleanup()

	// The outsider is not a teammate and is silently dropped from the list.
	rec := f.do(t, http.MethodPut, "/api/auth/remote-control/settings", f.owner.Email, map[string]interface{}{
		"enabled":          true,
		"allowed_user_ids": []string{f.friend.ID, f.outsider.ID, f.friend.ID, "nonsense"},
	})
	require.Equal(t, http.StatusOK, rec.Code, rec.Body.String())
	var saved struct {
		Enabled        bool     `json:"enabled"`
		AllowedUserIDs []string `json:"allowed_user_ids"`
	}
	require.NoError(t, json.Unmarshal(rec.Body.Bytes(), &saved))
	assert.True(t, saved.Enabled)
	assert.Equal(t, []string{f.friend.ID}, saved.AllowedUserIDs)

	// Allowed teammate: accepted (target app is offline, so nothing is delivered).
	rec = f.command(t, f.friend.Email, f.owner.ID, "start_share")
	require.Equal(t, http.StatusOK, rec.Code, rec.Body.String())
	var res remoteCommandResult
	require.NoError(t, json.Unmarshal(rec.Body.Bytes(), &res))
	assert.False(t, res.Online)
	assert.False(t, res.Delivered)

	// Teammate who is not on the list: refused.
	assert.Equal(t, http.StatusForbidden, f.command(t, f.bystander.Email, f.owner.ID, "start_share").Code)
	// User from another team: refused.
	assert.Equal(t, http.StatusForbidden, f.command(t, f.outsider.Email, f.owner.ID, "start_share").Code)
	// Unknown target: refused the same way.
	assert.Equal(t, http.StatusForbidden, f.command(t, f.friend.Email, "does-not-exist", "start_share").Code)
	// The allow-list is one-directional: the owner cannot control the friend.
	assert.Equal(t, http.StatusForbidden, f.command(t, f.owner.Email, f.friend.ID, "start_share").Code)
	// The owner's own account may control the owner's app once the feature is on.
	assert.Equal(t, http.StatusOK, f.command(t, f.owner.Email, f.owner.ID, "status").Code)

	// Unknown command.
	assert.Equal(t, http.StatusBadRequest, f.command(t, f.friend.Email, f.owner.ID, "format_disk").Code)

	// Even a hand-made row naming an outsider does not let them in (team check).
	require.NoError(t, f.srv.DB.Model(&models.RemoteControlSetting{}).Where("user_id = ?", f.owner.ID).
		Update("allowed_user_ids", `["`+f.outsider.ID+`"]`).Error)
	assert.Equal(t, http.StatusForbidden, f.command(t, f.outsider.Email, f.owner.ID, "start_share").Code)

	// Targets: the friend sees the owner, the bystander sees nobody.
	f.allow(t, true, f.friend)
	rec = f.do(t, http.MethodGet, "/api/auth/remote-control/targets", f.friend.Email, nil)
	require.Equal(t, http.StatusOK, rec.Code)
	var listed struct {
		Targets []struct {
			ID     string `json:"id"`
			IsSelf bool   `json:"is_self"`
			Online bool   `json:"online"`
		} `json:"targets"`
	}
	require.NoError(t, json.Unmarshal(rec.Body.Bytes(), &listed))
	require.Len(t, listed.Targets, 1)
	assert.Equal(t, f.owner.ID, listed.Targets[0].ID)
	assert.False(t, listed.Targets[0].IsSelf)
	assert.False(t, listed.Targets[0].Online)

	rec = f.do(t, http.MethodGet, "/api/auth/remote-control/targets", f.bystander.Email, nil)
	assert.JSONEq(t, `{"targets":[]}`, rec.Body.String())

	// Switching it off again refuses the friend at once.
	f.allow(t, false, f.friend)
	assert.Equal(t, http.StatusForbidden, f.command(t, f.friend.Email, f.owner.ID, "start_share").Code)
}

func TestRemoteControl_JoinRoomMustBelongToTeam(t *testing.T) {
	f, cleanup := newRemoteControlFixture(t)
	defer cleanup()
	f.allow(t, true, f.friend)

	foreign := &models.Room{Name: "Foreign", UserID: f.outsider.ID, TeamID: f.outsider.TeamID}
	require.NoError(t, f.srv.DB.Create(foreign).Error)
	own := &models.Room{Name: "Ours", UserID: f.owner.ID, TeamID: f.owner.TeamID}
	require.NoError(t, f.srv.DB.Create(own).Error)

	path := "/api/auth/remote-control/" + f.owner.ID + "/command"
	rec := f.do(t, http.MethodPost, path, f.friend.Email, map[string]interface{}{"command": "join", "room_id": foreign.ID})
	assert.Equal(t, http.StatusForbidden, rec.Code)
	rec = f.do(t, http.MethodPost, path, f.friend.Email, map[string]interface{}{"command": "join", "room_id": "no-such-room"})
	assert.Equal(t, http.StatusForbidden, rec.Code)
	rec = f.do(t, http.MethodPost, path, f.friend.Email, map[string]interface{}{"command": "join", "room_id": own.ID})
	assert.Equal(t, http.StatusOK, rec.Code, rec.Body.String())
}

// End to end over a real websocket: the command reaches the owner's (fake) app
// with the sender filled in by the server, and the app's answer comes back in
// the HTTP response.
func TestRemoteControl_EndToEndOverWebsocket(t *testing.T) {
	f, cleanup := newRemoteControlFixture(t)
	defer cleanup()
	f.allow(t, true, f.friend)

	httpSrv := httptest.NewServer(f.srv.Echo)
	t.Cleanup(httpSrv.Close)

	app := connectFakeApp(t, httpSrv, f.srv, f.owner.Email, func(payload map[string]interface{}) map[string]interface{} {
		return map[string]interface{}{
			"command_id": payload["command_id"],
			"ok":         true,
			"message":    "did " + payload["command"].(string),
			"status":     map[string]interface{}{"in_call": true, "sharing": payload["command"] == "start_share"},
		}
	})
	// A second connected user who must never see the owner's commands.
	bystanderApp := connectFakeApp(t, httpSrv, f.srv, f.bystander.Email, nil)
	waitOnline(t, f, f.owner)

	post := func(email, command string, extra map[string]interface{}) (*http.Response, remoteCommandResult) {
		body := map[string]interface{}{"command": command}
		for k, v := range extra {
			body[k] = v
		}
		raw, _ := json.Marshal(body)
		req, _ := http.NewRequest(http.MethodPost, httpSrv.URL+"/api/auth/remote-control/"+f.owner.ID+"/command", bytes.NewReader(raw))
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("Authorization", "Bearer "+getJWTToken(t, f.srv, email))
		resp, err := http.DefaultClient.Do(req)
		require.NoError(t, err)
		defer resp.Body.Close()
		var res remoteCommandResult
		_ = json.NewDecoder(resp.Body).Decode(&res)
		return resp, res
	}

	for _, cmd := range []string{"start_share", "stop_share", "join", "leave", "restart", "status"} {
		// from_user_id in the body is ignored: the sender always comes from the JWT.
		resp, res := post(f.friend.Email, cmd, map[string]interface{}{"from_user_id": f.owner.ID})
		require.Equal(t, http.StatusOK, resp.StatusCode, cmd)
		assert.True(t, res.Online, cmd)
		assert.True(t, res.Delivered, cmd)
		assert.True(t, res.OK, cmd)
		assert.Equal(t, "did "+cmd, res.Message)
		assert.Equal(t, true, res.Status["in_call"])

		select {
		case got := <-app.commands:
			assert.Equal(t, cmd, got["command"])
			assert.Equal(t, f.friend.ID, got["from_user_id"])
			assert.Equal(t, "Friend Test", got["from_name"])
			assert.NotEmpty(t, got["command_id"])
		case <-time.After(2 * time.Second):
			t.Fatalf("fake app never received %s", cmd)
		}
	}

	// A refused sender never reaches the app.
	resp, _ := post(f.bystander.Email, "start_share", nil)
	assert.Equal(t, http.StatusForbidden, resp.StatusCode)
	resp, _ = post(f.outsider.Email, "start_share", nil)
	assert.Equal(t, http.StatusForbidden, resp.StatusCode)
	select {
	case got := <-app.commands:
		t.Fatalf("app received a command from a refused sender: %v", got)
	case got := <-bystanderApp.commands:
		t.Fatalf("another user's app received a command: %v", got)
	case <-time.After(300 * time.Millisecond):
	}
}

// A client cannot inject a remote_command through its own websocket, and only
// the real target can answer a command.
func TestRemoteControl_NoInjectionNoSpoofedAnswer(t *testing.T) {
	f, cleanup := newRemoteControlFixture(t)
	defer cleanup()
	f.allow(t, true, f.friend)

	httpSrv := httptest.NewServer(f.srv.Echo)
	t.Cleanup(httpSrv.Close)

	// The bystander's app tries to answer in the owner's place as soon as it
	// learns the command id; the owner's app answers a little later.
	leaked := make(chan string, 4)
	bystanderApp := connectFakeApp(t, httpSrv, f.srv, f.bystander.Email, nil)
	ownerApp := connectFakeApp(t, httpSrv, f.srv, f.owner.Email, func(payload map[string]interface{}) map[string]interface{} {
		id := payload["command_id"].(string)
		leaked <- id
		time.Sleep(300 * time.Millisecond)
		return map[string]interface{}{"command_id": id, "ok": true, "message": "from the real app"}
	})
	waitOnline(t, f, f.owner)

	go func() {
		id := <-leaked
		_ = bystanderApp.conn.WriteJSON(map[string]interface{}{
			"type":    "remote_command_result",
			"payload": map[string]interface{}{"command_id": id, "ok": true, "message": "spoofed"},
		})
	}()

	// The bystander also tries to push a command straight at the owner.
	require.NoError(t, bystanderApp.conn.WriteJSON(map[string]interface{}{
		"type": "remote_command",
		"payload": map[string]interface{}{
			"command_id": "11111111-1111-1111-1111-111111111111", "command": "restart",
			"from_user_id": f.friend.ID, "from_name": "Friend Test", "issued_at": time.Now().Unix(),
		},
	}))

	raw, _ := json.Marshal(map[string]interface{}{"command": "leave"})
	req, _ := http.NewRequest(http.MethodPost, httpSrv.URL+"/api/auth/remote-control/"+f.owner.ID+"/command", bytes.NewReader(raw))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+getJWTToken(t, f.srv, f.friend.Email))
	resp, err := http.DefaultClient.Do(req)
	require.NoError(t, err)
	defer resp.Body.Close()
	var res remoteCommandResult
	require.NoError(t, json.NewDecoder(resp.Body).Decode(&res))
	assert.True(t, res.Delivered)
	assert.Equal(t, "from the real app", res.Message)

	// The owner's app saw exactly one command: the real "leave".
	select {
	case got := <-ownerApp.commands:
		assert.Equal(t, "leave", got["command"])
	case <-time.After(2 * time.Second):
		t.Fatal("owner app never received the real command")
	}
	select {
	case got := <-ownerApp.commands:
		t.Fatalf("owner app received an injected command: %v", got)
	case <-time.After(300 * time.Millisecond):
	}
}

// An app that never answers (an upstream/older build ignores the message).
func TestRemoteControl_AppDoesNotAnswer(t *testing.T) {
	f, cleanup := newRemoteControlFixture(t)
	defer cleanup()
	f.allow(t, true, f.friend)

	httpSrv := httptest.NewServer(f.srv.Echo)
	t.Cleanup(httpSrv.Close)
	app := connectFakeApp(t, httpSrv, f.srv, f.owner.Email, nil)
	waitOnline(t, f, f.owner)

	raw, _ := json.Marshal(map[string]interface{}{"command": "status"})
	req, _ := http.NewRequest(http.MethodPost, httpSrv.URL+"/api/auth/remote-control/"+f.owner.ID+"/command", bytes.NewReader(raw))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+getJWTToken(t, f.srv, f.friend.Email))
	resp, err := http.DefaultClient.Do(req)
	require.NoError(t, err)
	defer resp.Body.Close()
	var res remoteCommandResult
	require.NoError(t, json.NewDecoder(resp.Body).Decode(&res))
	assert.Equal(t, http.StatusOK, resp.StatusCode)
	assert.True(t, res.Online)
	assert.False(t, res.Delivered)
	assert.False(t, res.OK)
	assert.Len(t, app.commands, 1)
}

func TestRemoteControl_ControlPageIsServed(t *testing.T) {
	f, cleanup := newRemoteControlFixture(t)
	defer cleanup()

	// The page is resolved relative to the working directory (backend/ in the image).
	wd, err := os.Getwd()
	require.NoError(t, err)
	require.NoError(t, os.Chdir("../.."))
	defer func() { _ = os.Chdir(wd) }()

	rec := f.do(t, http.MethodGet, "/remote-control", "", nil)
	require.Equal(t, http.StatusOK, rec.Code)
	assert.Contains(t, rec.Body.String(), "Hopp remote control")
	assert.Contains(t, rec.Header().Get("Content-Type"), "text/html")
}
