package handlers

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"time"

	"hopp-backend/internal/common"
	"hopp-backend/internal/messages"
	"hopp-backend/internal/models"
	"hopp-backend/internal/redisutil"

	"github.com/google/uuid"
	"github.com/labstack/echo/v4"
	"gorm.io/gorm/clause"
)

// Remote commands let a teammate that a user explicitly allow-listed drive that
// user's desktop app from the web control page (/remote-control): start/stop the
// screen share, join/leave a call, restart the app.
//
// Safety model:
//   - Off by default. A user has to switch it on in the desktop app, which
//     mirrors the setting here (PUT /remote-control/settings).
//   - Only teammates on the user's allow-list (or the user themselves) may send
//     commands; this is checked here on every command and again by the app.
//   - The sender is taken from the JWT, never from the request body.
//   - Every command is logged with who, whom and what.
//
// The command travels over the target's existing websocket (their Redis
// channel); the app's answer comes back over the same websocket and is handed to
// the waiting HTTP request through a one-shot Redis channel.

const (
	RemoteCommandStatus     = "status"
	RemoteCommandStartShare = "start_share"
	RemoteCommandStopShare  = "stop_share"
	RemoteCommandJoin       = "join"
	RemoteCommandLeave      = "leave"
	RemoteCommandRestart    = "restart"

	// remoteCommandMaxPerWindow caps real commands (not status polls) a single
	// sender may issue per rateLimitWindow.
	remoteCommandMaxPerWindow = 30
	// remoteStatusMaxPerWindow caps status polls per sender per rateLimitWindow.
	remoteStatusMaxPerWindow = 120

	remoteCommandPendingTTL    = 30 * time.Second
	remoteCommandResultTimeout = 15 * time.Second
	remoteStatusResultTimeout  = 4 * time.Second

	remoteControlMaxAllowedUsers = 50
)

var validRemoteCommands = map[string]bool{
	RemoteCommandStatus:     true,
	RemoteCommandStartShare: true,
	RemoteCommandStopShare:  true,
	RemoteCommandJoin:       true,
	RemoteCommandLeave:      true,
	RemoteCommandRestart:    true,
}

const remoteControlDenied = "Remote control is not allowed for this user"

func remoteCommandPendingKey(commandID string) string {
	return "remote-control:pending:" + commandID
}

func remoteCommandResultChannel(commandID string) string {
	return "remote-control-result-" + commandID
}

type remoteControlSettingsRequest struct {
	Enabled        bool     `json:"enabled"`
	AllowedUserIDs []string `json:"allowed_user_ids"`
}

type remoteControlSettingsResponse struct {
	Enabled        bool     `json:"enabled"`
	AllowedUserIDs []string `json:"allowed_user_ids"`
}

// GetRemoteControlSettings returns the caller's own remote-command setting.
func (h *AuthHandler) GetRemoteControlSettings(c echo.Context) error {
	user, isAuthenticated := h.getAuthenticatedUserFromJWT(c)
	if !isAuthenticated {
		return c.String(http.StatusUnauthorized, "Unauthorized request")
	}

	setting, err := models.GetRemoteControlSetting(h.DB, user.ID)
	if err != nil {
		c.Logger().Error("remote-control: failed to load settings: ", err)
		return echo.NewHTTPError(http.StatusInternalServerError, "Failed to load remote control settings")
	}

	return c.JSON(http.StatusOK, remoteControlSettingsResponse{
		Enabled:        setting.Enabled,
		AllowedUserIDs: setting.AllowedUserIDs,
	})
}

// UpdateRemoteControlSettings stores the caller's own remote-command setting.
// Only teammates can be allow-listed; anything else is dropped.
func (h *AuthHandler) UpdateRemoteControlSettings(c echo.Context) error {
	user, isAuthenticated := h.getAuthenticatedUserFromJWT(c)
	if !isAuthenticated {
		return c.String(http.StatusUnauthorized, "Unauthorized request")
	}

	req := &remoteControlSettingsRequest{}
	if err := c.Bind(req); err != nil {
		return echo.NewHTTPError(http.StatusBadRequest, "Invalid request body")
	}
	if len(req.AllowedUserIDs) > remoteControlMaxAllowedUsers {
		return echo.NewHTTPError(http.StatusBadRequest, "Too many allowed users")
	}

	teammates, err := user.GetTeammates(h.DB)
	if err != nil {
		c.Logger().Error("remote-control: failed to load teammates: ", err)
		return echo.NewHTTPError(http.StatusInternalServerError, "Failed to load teammates")
	}
	isTeammate := make(map[string]bool, len(teammates))
	for _, teammate := range teammates {
		isTeammate[teammate.ID] = true
	}

	allowed := make([]string, 0, len(req.AllowedUserIDs))
	seen := make(map[string]bool, len(req.AllowedUserIDs))
	for _, id := range req.AllowedUserIDs {
		id = strings.TrimSpace(id)
		if id == "" || seen[id] || !isTeammate[id] {
			continue
		}
		seen[id] = true
		allowed = append(allowed, id)
	}

	setting := models.RemoteControlSetting{
		UserID:         user.ID,
		Enabled:        req.Enabled,
		AllowedUserIDs: allowed,
	}
	if err := h.DB.Clauses(clause.OnConflict{
		Columns:   []clause.Column{{Name: "user_id"}},
		DoUpdates: clause.AssignmentColumns([]string{"enabled", "allowed_user_ids", "updated_at"}),
	}).Create(&setting).Error; err != nil {
		c.Logger().Error("remote-control: failed to save settings: ", err)
		return echo.NewHTTPError(http.StatusInternalServerError, "Failed to save remote control settings")
	}

	c.Logger().Infof("remote-control: settings updated user=%s (%s) enabled=%t allowed=%v", user.ID, user.Email, req.Enabled, allowed)

	return c.JSON(http.StatusOK, remoteControlSettingsResponse{
		Enabled:        setting.Enabled,
		AllowedUserIDs: allowed,
	})
}

type remoteControlTarget struct {
	ID        string `json:"id"`
	FirstName string `json:"first_name"`
	LastName  string `json:"last_name"`
	IsSelf    bool   `json:"is_self"`
	Online    bool   `json:"online"`
}

// GetRemoteControlTargets lists the users whose desktop app the caller may
// control right now: teammates that switched the feature on and allow-listed the
// caller, plus the caller's own account when it has the feature on.
func (h *AuthHandler) GetRemoteControlTargets(c echo.Context) error {
	user, isAuthenticated := h.getAuthenticatedUserFromJWT(c)
	if !isAuthenticated {
		return c.String(http.StatusUnauthorized, "Unauthorized request")
	}

	teammates, err := user.GetTeammates(h.DB)
	if err != nil {
		c.Logger().Error("remote-control: failed to load teammates: ", err)
		return echo.NewHTTPError(http.StatusInternalServerError, "Failed to load teammates")
	}

	candidates := make(map[string]models.UserProfile, len(teammates)+1)
	ids := make([]string, 0, len(teammates)+1)
	candidates[user.ID] = *user
	ids = append(ids, user.ID)
	for _, teammate := range teammates {
		candidates[teammate.ID] = teammate.UserProfile
		ids = append(ids, teammate.ID)
	}

	var settings []models.RemoteControlSetting
	if err := h.DB.Where("user_id IN ? AND enabled = ?", ids, true).Find(&settings).Error; err != nil {
		c.Logger().Error("remote-control: failed to load settings: ", err)
		return echo.NewHTTPError(http.StatusInternalServerError, "Failed to load remote control settings")
	}

	targets := make([]remoteControlTarget, 0, len(settings))
	for i := range settings {
		setting := &settings[i]
		if !setting.Allows(user.ID) {
			continue
		}
		profile := candidates[setting.UserID]
		targets = append(targets, remoteControlTarget{
			ID:        profile.ID,
			FirstName: profile.FirstName,
			LastName:  profile.LastName,
			IsSelf:    profile.ID == user.ID,
			Online:    h.isUserOnline(c.Request().Context(), profile.ID),
		})
	}

	return c.JSON(http.StatusOK, map[string]interface{}{"targets": targets})
}

func (h *AuthHandler) isUserOnline(ctx context.Context, userID string) bool {
	if h.Redis == nil {
		return false
	}
	channels, err := h.Redis.PubSubChannels(ctx, redisutil.GetUserChannel(userID)).Result()
	return err == nil && len(channels) > 0
}

// authorizeRemoteCommand is the server-side gate for every remote command: the
// target must exist, be a teammate of the sender (or the sender), have the
// feature switched on and list the sender. The reason is for the log only;
// callers answer every refusal with the same message so nothing can be probed.
func authorizeRemoteCommand(s *common.ServerState, controller *models.UserProfile, targetID string) (*models.User, string) {
	if controller == nil || targetID == "" {
		return nil, "missing sender or target"
	}
	target, err := models.GetUserByID(s.DB, targetID)
	if err != nil || target == nil {
		return nil, "target not found"
	}
	if target.ID != controller.ID && !sameTeam(controller.TeamID, target.TeamID) {
		return nil, "not a teammate"
	}
	setting, err := models.GetRemoteControlSetting(s.DB, target.ID)
	if err != nil {
		return nil, "settings lookup failed"
	}
	if !setting.Enabled {
		return nil, "feature is off for target"
	}
	if !setting.Allows(controller.ID) {
		return nil, "sender is not on the target's allow-list"
	}
	return target, ""
}

type remoteCommandRequest struct {
	Command string `json:"command"`
	RoomID  string `json:"room_id"`
}

type remoteCommandResponse struct {
	// Online: the target's desktop app is connected to this server.
	Online bool `json:"online"`
	// Delivered: the app answered the command.
	Delivered bool `json:"delivered"`
	// OK / Message / Status: the app's own answer.
	OK      bool            `json:"ok"`
	Message string          `json:"message,omitempty"`
	Status  json.RawMessage `json:"status,omitempty"`
}

// SendRemoteCommand forwards one remote command to :userId's desktop app and
// waits for the app's answer.
func (h *AuthHandler) SendRemoteCommand(c echo.Context) error {
	controller, isAuthenticated := h.getAuthenticatedUserFromJWT(c)
	if !isAuthenticated {
		return c.String(http.StatusUnauthorized, "Unauthorized request")
	}

	targetID := c.Param("userId")

	req := &remoteCommandRequest{}
	if err := c.Bind(req); err != nil {
		return echo.NewHTTPError(http.StatusBadRequest, "Invalid request body")
	}
	if !validRemoteCommands[req.Command] {
		return echo.NewHTTPError(http.StatusBadRequest, "Unknown command")
	}
	isStatus := req.Command == RemoteCommandStatus

	target, denyReason := authorizeRemoteCommand(&h.ServerState, controller, targetID)
	if target == nil {
		c.Logger().Warnf("remote-control: DENIED command=%s from=%s (%s) to=%s reason=%s", req.Command, controller.ID, controller.Email, targetID, denyReason)
		return echo.NewHTTPError(http.StatusForbidden, remoteControlDenied)
	}

	if h.Redis == nil {
		return echo.NewHTTPError(http.StatusServiceUnavailable, "Remote control is unavailable")
	}
	ctx := c.Request().Context()

	// A room to join must belong to the same team as both users.
	roomID := ""
	if req.Command == RemoteCommandJoin && strings.TrimSpace(req.RoomID) != "" {
		room, err := models.GetRoomByID(h.DB, strings.TrimSpace(req.RoomID))
		if err != nil || room == nil || !sameTeam(room.TeamID, target.TeamID) || !sameTeam(room.TeamID, controller.TeamID) {
			c.Logger().Warnf("remote-control: DENIED command=join from=%s (%s) to=%s reason=room %q is not a room of the team", controller.ID, controller.Email, target.ID, req.RoomID)
			return echo.NewHTTPError(http.StatusForbidden, "You don't have access to this room")
		}
		roomID = room.ID
	}

	// Throttle per sender (fixed window, fails open like the other limiters).
	scope, limit := "remote-command", int64(remoteCommandMaxPerWindow)
	if isStatus {
		scope, limit = "remote-status", int64(remoteStatusMaxPerWindow)
	}
	if count, ttl, err := incrementRateLimit(ctx, h.Redis, fmt.Sprintf("ratelimit:%s:%s", scope, controller.ID)); err == nil && count > limit {
		c.Response().Header().Set("Retry-After", fmt.Sprintf("%d", int(ttl.Seconds())+1))
		return echo.NewHTTPError(http.StatusTooManyRequests, "Too many remote commands, slow down")
	}

	if !h.isUserOnline(ctx, target.ID) {
		if !isStatus {
			c.Logger().Infof("remote-control: command=%s from=%s (%s) to=%s (%s) result=target offline", req.Command, controller.ID, controller.Email, target.ID, target.Email)
		}
		return c.JSON(http.StatusOK, remoteCommandResponse{Online: false, Message: "Their Hopp app is not connected"})
	}

	commandID := uuid.New().String()

	// Subscribe for the answer before the command goes out so it cannot be missed.
	resultSub := h.Redis.Subscribe(ctx, remoteCommandResultChannel(commandID))
	defer resultSub.Close()
	if _, err := resultSub.Receive(ctx); err != nil {
		c.Logger().Error("remote-control: failed to subscribe for the result: ", err)
		return echo.NewHTTPError(http.StatusServiceUnavailable, "Remote control is unavailable")
	}

	// Only the target's own websocket may answer this command.
	if err := h.Redis.Set(ctx, remoteCommandPendingKey(commandID), target.ID, remoteCommandPendingTTL).Err(); err != nil {
		c.Logger().Error("remote-control: failed to record the pending command: ", err)
		return echo.NewHTTPError(http.StatusServiceUnavailable, "Remote control is unavailable")
	}
	defer h.Redis.Del(context.Background(), remoteCommandPendingKey(commandID))

	msg := messages.NewRemoteCommandMessage(messages.RemoteCommandPayload{
		CommandID:  commandID,
		Command:    req.Command,
		FromUserID: controller.ID,
		FromName:   controller.GetDisplayName(),
		RoomID:     roomID,
		IssuedAt:   time.Now().Unix(),
	})
	msgJSON, err := json.Marshal(msg)
	if err != nil {
		return echo.NewHTTPError(http.StatusInternalServerError, "Failed to build the command")
	}
	if err := h.Redis.Publish(ctx, redisutil.GetUserChannel(target.ID), msgJSON).Err(); err != nil {
		c.Logger().Error("remote-control: failed to publish the command: ", err)
		return echo.NewHTTPError(http.StatusServiceUnavailable, "Remote control is unavailable")
	}

	if !isStatus {
		c.Logger().Infof("remote-control: SENT command=%s id=%s from=%s (%s) to=%s (%s) room=%q", req.Command, commandID, controller.ID, controller.Email, target.ID, target.Email, roomID)
	}

	timeout := remoteCommandResultTimeout
	if isStatus {
		timeout = remoteStatusResultTimeout
	}
	timer := time.NewTimer(timeout)
	defer timer.Stop()

	select {
	case redisMsg, ok := <-resultSub.Channel():
		if !ok {
			return echo.NewHTTPError(http.StatusServiceUnavailable, "Remote control is unavailable")
		}
		var result messages.RemoteCommandResultPayload
		if err := json.Unmarshal([]byte(redisMsg.Payload), &result); err != nil {
			c.Logger().Error("remote-control: invalid result payload: ", err)
			return echo.NewHTTPError(http.StatusBadGateway, "The app sent an invalid answer")
		}
		if !isStatus {
			c.Logger().Infof("remote-control: RESULT command=%s id=%s from=%s to=%s ok=%t message=%q", req.Command, commandID, controller.ID, target.ID, result.OK, result.Message)
		}
		return c.JSON(http.StatusOK, remoteCommandResponse{
			Online:    true,
			Delivered: true,
			OK:        result.OK,
			Message:   result.Message,
			Status:    result.Status,
		})
	case <-timer.C:
		if !isStatus {
			c.Logger().Warnf("remote-control: NO ANSWER command=%s id=%s from=%s to=%s", req.Command, commandID, controller.ID, target.ID)
		}
		return c.JSON(http.StatusOK, remoteCommandResponse{
			Online:  true,
			Message: "Their Hopp app did not answer (it may be an older version, or remote control is switched off on that computer)",
		})
	case <-ctx.Done():
		return ctx.Err()
	}
}

// handleRemoteCommandResult takes a desktop app's answer from its websocket and
// hands it to the HTTP request that is waiting for it. Answers are only accepted
// from the user the command was sent to, once.
func handleRemoteCommandResult(ctx echo.Context, s *common.ServerState, userID string, msg messages.RemoteCommandResultMessage) {
	commandID := msg.Payload.CommandID
	if _, err := uuid.Parse(commandID); err != nil {
		ctx.Logger().Warnf("remote-control: dropped result with an invalid command id from user=%s", userID)
		return
	}

	bgCtx := context.Background()
	// Read first and delete only on a match, so an answer from anybody else can
	// never consume the slot that belongs to the real target.
	pendingTargetID, err := s.Redis.Get(bgCtx, remoteCommandPendingKey(commandID)).Result()
	if err != nil || pendingTargetID != userID {
		ctx.Logger().Warnf("remote-control: dropped result without a matching pending command id=%s from user=%s", commandID, userID)
		return
	}
	if deleted, err := s.Redis.Del(bgCtx, remoteCommandPendingKey(commandID)).Result(); err != nil || deleted == 0 {
		// Already answered once.
		return
	}

	// Keep answers small; the status snapshot is a handful of fields.
	if len(msg.Payload.Message) > 500 {
		msg.Payload.Message = msg.Payload.Message[:500]
	}
	if len(msg.Payload.Status) > 4096 {
		msg.Payload.Status = nil
	}

	payloadJSON, err := json.Marshal(msg.Payload)
	if err != nil {
		ctx.Logger().Error(err)
		return
	}
	s.Redis.Publish(bgCtx, remoteCommandResultChannel(commandID), payloadJSON)
}
