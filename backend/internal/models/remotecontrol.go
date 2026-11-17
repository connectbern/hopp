package models

import (
	"errors"
	"time"

	"gorm.io/gorm"
)

// RemoteControlSetting is the per-user opt-in for "remote commands": letting
// explicitly listed teammates drive this user's desktop app (start/stop the
// screen share, join/leave a call, restart the app) from the web control page.
//
// A user without a row has the feature off. The desktop app is the source of
// truth and mirrors its local setting here so the server can refuse commands
// before they ever reach the app; the app re-checks every command it receives.
type RemoteControlSetting struct {
	UserID         string    `gorm:"primaryKey" json:"user_id"`
	Enabled        bool      `gorm:"not null" json:"enabled"`
	AllowedUserIDs []string  `gorm:"serializer:json" json:"allowed_user_ids"`
	CreatedAt      time.Time `json:"created_at"`
	UpdatedAt      time.Time `json:"updated_at"`
}

// GetRemoteControlSetting returns the user's setting, or a disabled, empty
// setting when the user never configured one.
func GetRemoteControlSetting(db *gorm.DB, userID string) (*RemoteControlSetting, error) {
	var setting RemoteControlSetting
	err := db.Where("user_id = ?", userID).First(&setting).Error
	if errors.Is(err, gorm.ErrRecordNotFound) {
		return &RemoteControlSetting{UserID: userID, AllowedUserIDs: []string{}}, nil
	}
	if err != nil {
		return nil, err
	}
	if setting.AllowedUserIDs == nil {
		setting.AllowedUserIDs = []string{}
	}
	return &setting, nil
}

// Allows reports whether controllerID may send remote commands to the owner of
// this setting. The owner may always control their own app once the feature is
// on; everybody else must be listed explicitly.
func (s *RemoteControlSetting) Allows(controllerID string) bool {
	if s == nil || !s.Enabled || controllerID == "" {
		return false
	}
	if controllerID == s.UserID {
		return true
	}
	for _, id := range s.AllowedUserIDs {
		if id == controllerID {
			return true
		}
	}
	return false
}
